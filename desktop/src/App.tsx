import { useCallback, useEffect, useId, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { open as openFolderDialog, confirm as confirmDialog } from "@tauri-apps/plugin-dialog";
import {
  api,
  AccountSummary,
  NamespaceSummary,
  OperationSummary,
  ConflictSummary,
  CacheEntry,
  NamespaceItem,
  PinState,
  ConflictResolutions,
  ConflictResolution,
  IgnoreRule,
  IgnoreProfileSuggestion,
  CloudProviders,
} from "./api";
import { ToastProvider, useAction } from "./toast";
import logoLight from "./assets/logo.png";
import logoDark from "./assets/logo-dark.png";

/** T5-02: qualquer evento do daemon cujo `type` esteja em `refetchOn` dispara
 * uma releitura da lista — é isso que torna as telas "em tempo real sem
 * polling" (SPEC §20.4) em vez de um `setInterval`. Também recarrega em
 * `nexofs://connected` (emitido toda vez que o backend Tauri estabelece —
 * ou reestabelece — o stream SSE com o daemon): cobre tanto "a UI abriu
 * antes do `nexofsd` estar de pé" quanto "o daemon caiu e voltou", nenhum
 * dos dois casos tem um `SyncEvent` específico para reagir. Sem isso, uma
 * seção que erra na carga inicial ficava presa nesse erro para sempre. */
function useApiList<T>(fetcher: () => Promise<T>, refetchOn: string[]) {
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);

  const reload = useCallback(() => {
    fetcher()
      .then((value) => {
        setData(value);
        setError(null);
      })
      .catch((err) => setError(String(err)))
      .finally(() => setLoading(false));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [fetcher]);

  useEffect(() => {
    setLoading(true);
    reload();
  }, [reload]);

  useEffect(() => {
    const unlistenEvent = listen<{ type: string }>("nexofs://event", (event) => {
      if (refetchOn.includes(event.payload.type)) reload();
    });
    const unlistenConnected = listen("nexofs://connected", () => reload());
    return () => {
      unlistenEvent.then((stop) => stop());
      unlistenConnected.then((stop) => stop());
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [reload, refetchOn.join(",")]);

  return { data, error, loading, reload };
}

const RETRYABLE_STATES = new Set(["WaitingRetry", "WaitingNetwork", "WaitingAuthentication", "FailedPermanent"]);
const CANCELLABLE_STATES = new Set(["Pending", "WaitingRetry", "WaitingNetwork", "WaitingAuthentication"]);

const STATE_LABELS: Record<string, string> = {
  Pending: "Na fila",
  Running: "Em andamento",
  WaitingRetry: "Aguardando nova tentativa",
  WaitingNetwork: "Sem rede",
  WaitingAuthentication: "Requer login",
  BlockedByConflict: "Bloqueado por conflito",
  Completed: "Concluída",
  Cancelled: "Cancelada",
  FailedPermanent: "Falhou",
};

function bytesToHuman(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value.toFixed(1)} ${units[unit]}`;
}

function Card({ title, action, children }: { title: string; action?: React.ReactNode; children: React.ReactNode }) {
  return (
    <section className="card">
      <div className="card-header">
        <h2>{title}</h2>
        {action}
      </div>
      <div className="card-body">{children}</div>
    </section>
  );
}

function EmptyRow({ colSpan, children }: { colSpan: number; children: React.ReactNode }) {
  return (
    <tr>
      <td colSpan={colSpan} className="empty-row">
        {children}
      </td>
    </tr>
  );
}

/** T6-10 (acessibilidade): `role="dialog"`/`aria-modal` para leitores de
 * tela anunciarem isto como um diálogo (não só mais uma `div` na página);
 * Esc fecha, igual a qualquer diálogo nativo do sistema; o foco vai para o
 * próprio painel ao abrir (`tabIndex={-1}` — recebe foco por script sem
 * entrar na ordem de Tab normal) para quem navega por teclado não continuar
 * "preso" onde estava atrás do diálogo. */
function Modal({ title, onClose, children }: { title: string; onClose: () => void; children: React.ReactNode }) {
  const panelRef = useRef<HTMLDivElement>(null);
  const titleId = useId();

  useEffect(() => {
    panelRef.current?.focus();
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    document.addEventListener("keydown", onKeyDown);
    return () => document.removeEventListener("keydown", onKeyDown);
  }, [onClose]);

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal" role="dialog" aria-modal="true" aria-labelledby={titleId} tabIndex={-1} ref={panelRef} onClick={(e) => e.stopPropagation()}>
        <div className="modal-header">
          <h2 id={titleId}>{title}</h2>
          <button className="link-button" onClick={onClose} aria-label="Fechar">
            ✕
          </button>
        </div>
        <div className="modal-body">{children}</div>
      </div>
    </div>
  );
}

/** T5-desktop ("nome/local da montagem"): abre o navegador só depois que o
 * usuário confirma nome e pasta — antes disso é só um formulário local, sem
 * nenhuma chamada ao daemon. */
function AddAccountDialog({ onClose, onAdded }: { onClose: () => void; onAdded: () => void }) {
  const [providerId, setProviderId] = useState<string>(CloudProviders[0].id);
  const [displayName, setDisplayName] = useState("");
  const [mountPath, setMountPath] = useState("");
  const addAccount = useAction((provider: string, name: string, path: string) => api.addAccount(provider, path || undefined, name || undefined), {
    successMessage: "Conta adicionada e montada.",
    onSuccess: () => {
      onAdded();
      onClose();
    },
  });

  async function pickFolder() {
    const chosen = await openFolderDialog({ directory: true, multiple: false, title: "Escolher pasta para a montagem" });
    if (typeof chosen === "string") setMountPath(chosen);
  }

  return (
    <Modal title="Adicionar conta" onClose={onClose}>
      <p className="hint">
        O navegador do sistema vai abrir para você fazer login. Antes disso, escolha o provedor e (opcionalmente) o nome e a pasta onde esta conta será
        montada — se deixar em branco, o NexoFS usa um padrão sensato.
      </p>
      <label className="field">
        Provedor
        <select value={providerId} onChange={(e) => setProviderId(e.target.value)} disabled={addAccount.pending}>
          {CloudProviders.map((p) => (
            <option key={p.id} value={p.id}>
              {p.label}
            </option>
          ))}
        </select>
      </label>
      <label className="field">
        Nome da montagem
        <input value={displayName} onChange={(e) => setDisplayName(e.target.value)} placeholder="Ex.: OneDrive Pessoal" disabled={addAccount.pending} />
      </label>
      <label className="field">
        Local no disco
        <div className="actions">
          <input
            value={mountPath}
            onChange={(e) => setMountPath(e.target.value)}
            placeholder="Padrão: ~/NexoFS/<nome>"
            disabled={addAccount.pending}
          />
          <button onClick={pickFolder} disabled={addAccount.pending}>
            Escolher pasta…
          </button>
        </div>
      </label>
      <div className="actions modal-footer-actions">
        <button onClick={onClose} disabled={addAccount.pending}>
          Cancelar
        </button>
        <button className="btn-primary" onClick={() => addAccount.run(providerId, displayName, mountPath)} disabled={addAccount.pending}>
          {addAccount.pending ? "Aguardando login no navegador…" : "Conectar"}
        </button>
      </div>
    </Modal>
  );
}

function HelpModal({ onClose }: { onClose: () => void }) {
  return (
    <Modal title="Como o NexoFS funciona" onClose={onClose}>
      <div className="help-content">
        <h3>Contas</h3>
        <p>
          Cada conta OneDrive vira uma pasta no seu computador. "Desmontar" tira a pasta do ar sem esquecer a conta (dá para remontar depois); "Excluir"
          apaga a conta do NexoFS por completo — os arquivos já sincronizados continuam no disco, só param de ser gerenciados.
        </p>
        <h3>Arquivos</h3>
        <p>
          Navegue pelas pastas de cada conta e escolha o que deve ficar <strong>sempre disponível no disco</strong> (fixado — útil para o que outro
          programa precisa acessar mesmo sem internet) e o que pode voltar a ser <strong>só online</strong> (baixado sob demanda, economizando espaço).
        </p>
        <h3>Exclusões</h3>
        <p>
          Pastas/arquivos que nunca sincronizam (ex.: <code>node_modules/</code>). Aceite um perfil sugerido quando detectarmos um projeto conhecido, ou
          adicione seu próprio padrão. Cada regra mostra de onde veio.
        </p>
        <h3>Operações</h3>
        <p>Fila de uploads/downloads pendentes. Itens travados por erro podem ser repetidos ou cancelados manualmente.</p>
        <h3>Conflitos</h3>
        <p>Aparecem quando o mesmo arquivo mudou nos dois lados. Escolha qual versão vale, ou mantenha as duas.</p>
        <h3>Cache</h3>
        <p>Quanto espaço em disco cada conta está usando com arquivos baixados. "Aplicar quota agora" libera espaço de itens não fixados.</p>
        <h3>Log</h3>
        <p>Acompanha em tempo real o que está sendo sincronizado, conta por conta, enquanto esta janela estiver aberta.</p>
      </div>
    </Modal>
  );
}

const MOUNT_STATE_LABELS: Record<string, string> = {
  MOUNTED: "Montada",
  UNMOUNTED: "Desmontada",
  MOUNTING: "Montando…",
};

interface KebabMenuItem {
  label: string;
  onClick: () => void;
  disabled?: boolean;
  danger?: boolean;
}

/** Menu "⋮" para substituir uma fileira de botões por linha de tabela.
 * O dropdown usa `position: fixed` com coordenadas calculadas em JS (não
 * `position: absolute` + CSS) porque `.table-scroll` tem `overflow-x: auto`
 * — pela regra do CSS, isso força `overflow-y` a computar como `auto`
 * também, cortando qualquer overlay absoluto que ultrapasse a última linha
 * visível da tabela. `position: fixed` escapa desse corte por não ter
 * `.table-scroll` como containing block. */
function KebabMenu({ items }: { items: KebabMenuItem[] }) {
  const [open, setOpen] = useState(false);
  const [pos, setPos] = useState<{ top: number; left: number } | null>(null);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    function onPointerDown(e: MouseEvent) {
      const target = e.target as Node;
      if (menuRef.current?.contains(target) || triggerRef.current?.contains(target)) return;
      setOpen(false);
    }
    function onKeyDown(e: KeyboardEvent) {
      if (e.key === "Escape") setOpen(false);
    }
    document.addEventListener("mousedown", onPointerDown);
    document.addEventListener("keydown", onKeyDown);
    return () => {
      document.removeEventListener("mousedown", onPointerDown);
      document.removeEventListener("keydown", onKeyDown);
    };
  }, [open]);

  function toggle() {
    if (!open && triggerRef.current) {
      const rect = triggerRef.current.getBoundingClientRect();
      const menuWidth = 190;
      setPos({ top: rect.bottom + 4, left: Math.max(8, rect.right - menuWidth) });
    }
    setOpen((o) => !o);
  }

  return (
    <>
      <button ref={triggerRef} className="kebab-trigger" onClick={toggle} aria-haspopup="true" aria-expanded={open} aria-label="Mais opções" title="Mais opções">
        ⋮
      </button>
      {open && pos && (
        <div ref={menuRef} className="kebab-dropdown" role="menu" style={{ top: pos.top, left: pos.left }}>
          {items.map((item) => (
            <button
              key={item.label}
              role="menuitem"
              className={`kebab-item ${item.danger ? "kebab-item-danger" : ""}`}
              disabled={item.disabled}
              onClick={() => {
                setOpen(false);
                item.onClick();
              }}
            >
              {item.label}
            </button>
          ))}
        </div>
      )}
    </>
  );
}

function AccountsAndNamespaces({ onAccountAdded }: { onAccountAdded: () => void }) {
  const [showAddDialog, setShowAddDialog] = useState(false);
  const accounts = useApiList<{ accounts: AccountSummary[] }>(api.accounts, []);
  const namespaces = useApiList<{ namespaces: NamespaceSummary[] }>(api.namespaces, ["NAMESPACE_MOUNTED", "NAMESPACE_UNMOUNTED"]);

  const reloadBoth = () => {
    accounts.reload();
    namespaces.reload();
  };

  const refreshAction = useAction(api.refreshNamespace, { successMessage: "Atualização concluída." });
  const syncAction = useAction(api.syncNow, { successMessage: "Sincronização disparada." });
  const unmountAction = useAction(api.unmountAccount, { successMessage: "Conta desmontada.", onSuccess: reloadBoth });
  const remountAction = useAction(api.remountAccount, { successMessage: "Conta remontada.", onSuccess: reloadBoth });
  const deleteAction = useAction(api.deleteAccount, { successMessage: "Conta excluída.", onSuccess: reloadBoth });

  async function confirmDelete(displayName: string, accountId: string) {
    // Bug real encontrado ao vivo: `window.confirm` num webview do Tauri
    // não abre o confirm nativo do navegador — é interceptado e roteado
    // para o comando `dialog.message` do plugin, que sem a permissão certa
    // falhava silenciosamente (e, por ser assíncrono, um `if
    // (window.confirm(...))` nunca esperava a resposta de verdade). A API
    // do próprio `@tauri-apps/plugin-dialog` é a forma correta de pedir
    // confirmação aqui.
    const confirmed = await confirmDialog(`Excluir a conta "${displayName}"? Os arquivos já sincronizados continuam no disco, mas o NexoFS para de gerenciá-los.`, {
      title: "Excluir conta",
      kind: "warning",
    });
    if (confirmed) {
      deleteAction.run(accountId);
    }
  }

  return (
    <Card
      title="Contas e montagens"
      action={
        <button className="btn-primary" onClick={() => setShowAddDialog(true)}>
          + Adicionar conta
        </button>
      }
    >
      {showAddDialog && (
        <AddAccountDialog
          onClose={() => setShowAddDialog(false)}
          onAdded={() => {
            reloadBoth();
            onAccountAdded();
          }}
        />
      )}
      {accounts.error && <p className="error">{accounts.error}</p>}
      {namespaces.error && <p className="error">{namespaces.error}</p>}
      <div className="table-scroll">
        <table>
          <thead>
            <tr>
              <th>Conta</th>
              <th>Provedor</th>
              <th>Ponto de montagem</th>
              <th>Estado</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {namespaces.data?.namespaces.map((ns) => {
              const account = accounts.data?.accounts.find((a) => a.account_id === ns.account_id);
              const isMounted = ns.mount_state === "MOUNTED";
              return (
                <tr key={ns.namespace_id}>
                  <td>{ns.display_name}</td>
                  <td>{CloudProviders.find((p) => p.id === account?.provider_id)?.label ?? account?.provider_id ?? "—"}</td>
                  <td>
                    <code>{ns.mount_path}</code>
                  </td>
                  <td>
                    <span className={`badge ${isMounted ? "badge-mounted" : "badge-pin-online-only"}`}>{MOUNT_STATE_LABELS[ns.mount_state] ?? ns.mount_state}</span>
                  </td>
                  <td className="actions">
                    <KebabMenu
                      items={
                        isMounted
                          ? [
                              { label: "Atualizar", onClick: () => refreshAction.run(ns.namespace_id), disabled: refreshAction.pending },
                              { label: "Sincronizar agora", onClick: () => syncAction.run(ns.namespace_id), disabled: syncAction.pending },
                              { label: "Desmontar", onClick: () => unmountAction.run(ns.account_id), disabled: unmountAction.pending },
                              { label: "Excluir conta…", onClick: () => confirmDelete(ns.display_name, ns.account_id), disabled: deleteAction.pending, danger: true },
                            ]
                          : [
                              { label: "Remontar", onClick: () => remountAction.run(ns.account_id), disabled: remountAction.pending },
                              { label: "Excluir conta…", onClick: () => confirmDelete(ns.display_name, ns.account_id), disabled: deleteAction.pending, danger: true },
                            ]
                      }
                    />
                  </td>
                </tr>
              );
            })}
            {!namespaces.loading && namespaces.data?.namespaces.length === 0 && (
              <EmptyRow colSpan={5}>Nenhuma conta montada ainda — clique em "Adicionar conta".</EmptyRow>
            )}
          </tbody>
        </table>
      </div>
    </Card>
  );
}

function OperationsQueue() {
  const operations = useApiList<{ operations: OperationSummary[] }>(api.operations, ["OPERATION_PROGRESS"]);
  const retry = useAction((id: string) => api.retryOperation(id), { successMessage: "Nova tentativa agendada.", onSuccess: operations.reload });
  const cancel = useAction((id: string) => api.cancelOperation(id), { successMessage: "Operação cancelada.", onSuccess: operations.reload });

  return (
    <Card title="Fila de operações">
      {operations.error && <p className="error">{operations.error}</p>}
      <div className="table-scroll">
        <table>
          <thead>
            <tr>
              <th>Tipo</th>
              <th>Estado</th>
              <th>Tentativas</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {operations.data?.operations.map((op) => (
              <tr key={op.operation_id}>
                <td>{op.operation_type}</td>
                <td>
                  <span className={`badge badge-op-${op.state.toLowerCase()}`}>{STATE_LABELS[op.state] ?? op.state}</span>
                </td>
                <td>{op.attempt_count}</td>
                <td className="actions">
                  {RETRYABLE_STATES.has(op.state) && (
                    <button onClick={() => retry.run(op.operation_id)} disabled={retry.pending}>
                      Repetir agora
                    </button>
                  )}
                  {CANCELLABLE_STATES.has(op.state) && (
                    <button className="btn-danger" onClick={() => cancel.run(op.operation_id)} disabled={cancel.pending}>
                      Cancelar
                    </button>
                  )}
                </td>
              </tr>
            ))}
            {!operations.loading && operations.data?.operations.length === 0 && (
              <EmptyRow colSpan={4}>Nenhuma operação pendente — tudo sincronizado.</EmptyRow>
            )}
          </tbody>
        </table>
      </div>
    </Card>
  );
}

const CONFLICT_TYPE_LABELS: Record<string, string> = {
  CONTENT_CHANGED_BOTH_SIDES: "Editado nos dois lados",
  REMOTE_DELETED_LOCAL_MODIFIED: "Apagado na nuvem, mas editado localmente",
  LOCAL_DELETED_REMOTE_MODIFIED: "Apagado localmente, mas editado na nuvem",
  RENAME_COLLISION: "Renomeado nos dois lados de formas diferentes",
  MOVE_COLLISION: "Movido nos dois lados de formas diferentes",
  CASE_COLLISION: "Nomes diferindo só em maiúsculas/minúsculas",
  LOCAL_ONLY_REMOTE_COLLISION: "Arquivo só local colidiu com um novo na nuvem",
  PARENT_DELETED: "A pasta que contém este item foi apagada",
  UNSUPPORTED_NAME: "Nome não é aceito pelo provedor de nuvem",
};

const CONFLICT_RESOLUTION_LABELS: Record<ConflictResolution, string> = {
  KEEP_LOCAL: "Manter versão local",
  KEEP_REMOTE: "Manter versão da nuvem",
  KEEP_BOTH: "Manter as duas (duplicar)",
  SAVE_LOCAL_ELSEWHERE: "Salvar a local em outro lugar",
  DISMISS_TEMPORARILY: "Ignorar por enquanto",
};

function ConflictResolutionPicker({ conflict, onResolved }: { conflict: ConflictSummary; onResolved: () => void }) {
  const [resolution, setResolution] = useState<ConflictResolution>("KEEP_LOCAL");
  const resolve = useAction((id: string, r: ConflictResolution) => api.resolveConflict(id, r), { successMessage: "Conflito resolvido.", onSuccess: onResolved });
  return (
    <div className="actions">
      <select value={resolution} onChange={(e) => setResolution(e.target.value as ConflictResolution)}>
        {ConflictResolutions.map((r) => (
          <option key={r} value={r}>
            {CONFLICT_RESOLUTION_LABELS[r]}
          </option>
        ))}
      </select>
      <button onClick={() => resolve.run(conflict.conflict_id, resolution)} disabled={resolve.pending}>
        {resolve.pending ? "Resolvendo…" : "Resolver"}
      </button>
    </div>
  );
}

function Conflicts() {
  const conflicts = useApiList<{ conflicts: ConflictSummary[] }>(api.conflicts, ["CONFLICT_CREATED", "CONFLICT_RESOLVED"]);

  return (
    <Card title="Conflitos abertos">
      {conflicts.error && <p className="error">{conflicts.error}</p>}
      <div className="table-scroll">
        <table>
          <thead>
            <tr>
              <th>Arquivo</th>
              <th>O que aconteceu</th>
              <th>Detectado em</th>
              <th>Resolução</th>
            </tr>
          </thead>
          <tbody>
            {conflicts.data?.conflicts.map((c) => (
              <tr key={c.conflict_id}>
                <td>
                  {c.item_path || c.item_name ? <code>/{c.item_path ?? c.item_name}</code> : <span className="text-muted">item removido (id {c.item_id.slice(0, 8)}…)</span>}
                </td>
                <td>{CONFLICT_TYPE_LABELS[c.conflict_type] ?? c.conflict_type}</td>
                <td>{new Date(c.detected_at * 1000).toLocaleString("pt-BR")}</td>
                <td>
                  <ConflictResolutionPicker conflict={c} onResolved={conflicts.reload} />
                </td>
              </tr>
            ))}
            {!conflicts.loading && conflicts.data?.conflicts.length === 0 && <EmptyRow colSpan={4}>Nenhum conflito aberto.</EmptyRow>}
          </tbody>
        </table>
      </div>
    </Card>
  );
}

function CacheUsage() {
  const namespaces = useApiList<{ namespaces: NamespaceSummary[] }>(api.namespaces, []);
  const cache = useApiList<{ cache: CacheEntry[]; max_bytes_per_namespace: number }>(api.cache, ["CACHE_PRESSURE_CHANGED"]);
  const cleanup = useAction(api.cleanupCache, { successMessage: "Quota de cache aplicada.", onSuccess: cache.reload });

  return (
    <Card
      title="Cache e espaço local"
      action={
        <button onClick={() => cleanup.run()} disabled={cleanup.pending}>
          {cleanup.pending ? "Aplicando…" : "Aplicar quota agora"}
        </button>
      }
    >
      {cache.error && <p className="error">{cache.error}</p>}
      {cache.data && (
        <p className="hint">Quota por conta: {bytesToHuman(cache.data.max_bytes_per_namespace)}.</p>
      )}
      <div className="table-scroll">
        <table>
          <thead>
            <tr>
              <th>Conta</th>
              <th>Total</th>
              <th>Limpo</th>
              <th>Modificado localmente</th>
              <th>Parcial</th>
              <th>Mantido localmente</th>
            </tr>
          </thead>
          <tbody>
            {cache.data?.cache.map((entry) => {
              const ns = namespaces.data?.namespaces.find((n) => n.namespace_id === entry.namespace_id);
              return (
                <tr key={entry.namespace_id}>
                  <td>{ns?.display_name ?? entry.namespace_id}</td>
                  <td>
                    {bytesToHuman(entry.hydrated_bytes)} ({entry.hydrated_items})
                  </td>
                  <td>{bytesToHuman(entry.clean_bytes)}</td>
                  <td>{bytesToHuman(entry.dirty_bytes)}</td>
                  <td>{bytesToHuman(entry.partial_bytes)}</td>
                  <td>{bytesToHuman(entry.overlay_bytes)}</td>
                </tr>
              );
            })}
            {!cache.loading && cache.data?.cache.length === 0 && <EmptyRow colSpan={6}>Nenhum dado de cache ainda.</EmptyRow>}
          </tbody>
        </table>
      </div>
    </Card>
  );
}

const PIN_LABELS: Record<PinState, string> = {
  ONLINE_ONLY: "Só online",
  AVAILABLE_LOCALLY: "Disponível localmente",
  PINNED: "Mantido no dispositivo",
};

/** T5-desktop (FR-PIN-001/002, "fixação seletiva"): navegador de árvore por
 * namespace com um botão por item para alternar entre "mantido sempre no
 * dispositivo" (fixado — não pode ficar Online-Only enquanto estiver aberto
 * por outro app) e "voltar a só online" — a mesma dualidade que Dropbox/
 * OneDrive chamam de "sempre manter neste dispositivo" vs. "liberar espaço". */
function FilesBrowser() {
  const namespaces = useApiList<{ namespaces: NamespaceSummary[] }>(api.namespaces, []);
  const [namespaceId, setNamespaceId] = useState<string>("");
  const [path, setPath] = useState<{ id?: string; name: string }[]>([{ id: undefined, name: "Raiz" }]);

  useEffect(() => {
    if (!namespaceId && namespaces.data?.namespaces.length) {
      setNamespaceId(namespaces.data.namespaces[0].namespace_id);
    }
  }, [namespaceId, namespaces.data]);

  const currentParentId = path[path.length - 1]?.id;
  const fetchItems = useCallback(() => {
    if (!namespaceId) return Promise.resolve({ parent_item_id: "", items: [] as NamespaceItem[] });
    return api.items(namespaceId, currentParentId);
  }, [namespaceId, currentParentId]);
  const items = useApiList(fetchItems, []);

  const togglePin = useAction(
    (item: NamespaceItem) => api.setPinState(namespaceId, item.item_id, item.pin_state === "PINNED" ? "ONLINE_ONLY" : "PINNED", item.kind === "Directory"),
    { onSuccess: items.reload },
  );

  function openFolder(item: NamespaceItem) {
    setPath((current) => [...current, { id: item.item_id, name: item.name }]);
  }

  function goToBreadcrumb(index: number) {
    setPath((current) => current.slice(0, index + 1));
  }

  function switchNamespace(id: string) {
    setNamespaceId(id);
    setPath([{ id: undefined, name: "Raiz" }]);
  }

  return (
    <Card
      title="Arquivos e fixação seletiva"
      action={
        namespaces.data && namespaces.data.namespaces.length > 1 ? (
          <select value={namespaceId} onChange={(e) => switchNamespace(e.target.value)}>
            {namespaces.data.namespaces.map((ns) => (
              <option key={ns.namespace_id} value={ns.namespace_id}>
                {ns.display_name}
              </option>
            ))}
          </select>
        ) : undefined
      }
    >
      <p className="hint">
        Fixe um arquivo ou pasta para mantê-lo sempre disponível no disco, mesmo offline — útil para o que outro programa precisa acessar a qualquer
        momento. Clique de novo para liberar espaço.
      </p>
      <nav className="breadcrumb">
        {path.map((crumb, index) => (
          <span key={index}>
            {index > 0 && <span className="breadcrumb-sep">/</span>}
            <button className="breadcrumb-item" onClick={() => goToBreadcrumb(index)} disabled={index === path.length - 1}>
              {crumb.name}
            </button>
          </span>
        ))}
      </nav>
      {items.error && <p className="error">{items.error}</p>}
      <div className="table-scroll">
        <table>
          <thead>
            <tr>
              <th>Nome</th>
              <th>Tamanho</th>
              <th>Disponibilidade</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {items.data?.items.map((item) => (
              <tr key={item.item_id}>
                <td>
                  {item.kind === "Directory" ? (
                    <button className="link-button" onClick={() => openFolder(item)}>
                      📁 {item.name}
                    </button>
                  ) : (
                    <span>📄 {item.name}</span>
                  )}
                </td>
                <td>{item.kind === "File" ? bytesToHuman(item.size_bytes) : "—"}</td>
                <td>
                  <span className={`badge badge-pin-${item.pin_state.toLowerCase().replace(/_/g, "-")}`}>{PIN_LABELS[item.pin_state]}</span>
                </td>
                <td className="actions">
                  <button onClick={() => togglePin.run(item)} disabled={togglePin.pending}>
                    {item.pin_state === "PINNED" ? "Voltar para online" : "Manter no dispositivo"}
                  </button>
                </td>
              </tr>
            ))}
            {!items.loading && items.data?.items.length === 0 && <EmptyRow colSpan={4}>Pasta vazia.</EmptyRow>}
          </tbody>
        </table>
      </div>
    </Card>
  );
}

/** T5-06 (FR-IGN-003, "ver origem"): a camada (`tier`) vem direto do banco —
 * `RuleTier::Account` para o que o usuário adiciona por aqui, mas uma regra
 * aplicada por um perfil sugerido (T4-04) aparece como `TechProfile`, e
 * assim por diante — é a transparência que a SPEC pede, não um rótulo
 * inventado no frontend. */
const IGNORE_TIER_LABELS: Record<string, string> = {
  Defaults: "Padrão do NexoFS",
  AdminPolicy: "Política do administrador",
  TechProfile: "Perfil de tecnologia",
  UserGlobal: "Regra global do usuário",
  Account: "Esta conta",
  Folder: "Pasta específica",
  NexofsIgnoreFile: ".nexofsignore",
  UserException: "Exceção do usuário",
};

/** T5-06: visualizar/criar/remover regras de exclusão e aplicar perfis de
 * tecnologia sugeridos — o núcleo (`nexofs-ignore`/`SyncCore`) já existe
 * desde a Fase 4, esta tela só dá acesso a ele. */
function IgnoreRulesTab() {
  const namespaces = useApiList<{ namespaces: NamespaceSummary[] }>(api.namespaces, []);
  const [namespaceId, setNamespaceId] = useState<string>("");
  const [pattern, setPattern] = useState("");

  useEffect(() => {
    if (!namespaceId && namespaces.data?.namespaces.length) {
      setNamespaceId(namespaces.data.namespaces[0].namespace_id);
    }
  }, [namespaceId, namespaces.data]);

  const fetchRules = useCallback(() => {
    if (!namespaceId) return Promise.resolve({ rules: [] as IgnoreRule[] });
    return api.ignoreRules(namespaceId);
  }, [namespaceId]);
  const rules = useApiList(fetchRules, []);

  const fetchSuggestions = useCallback(() => {
    if (!namespaceId) return Promise.resolve({ suggestions: [] as IgnoreProfileSuggestion[] });
    return api.ignoreProfileSuggestions(namespaceId);
  }, [namespaceId]);
  const suggestions = useApiList(fetchSuggestions, []);

  const addRule = useAction((p: string) => api.addIgnoreRule(namespaceId, p), {
    successMessage: "Regra adicionada.",
    onSuccess: () => {
      setPattern("");
      rules.reload();
    },
  });
  const removeRule = useAction((ruleId: string) => api.removeIgnoreRule(namespaceId, ruleId), {
    successMessage: "Regra removida.",
    onSuccess: rules.reload,
  });
  const applyProfile = useAction((manifestFile: string) => api.applyIgnoreProfile(namespaceId, manifestFile), {
    successMessage: "Perfil aplicado.",
    onSuccess: () => {
      rules.reload();
      suggestions.reload();
    },
  });

  return (
    <Card
      title="Exclusões"
      action={
        namespaces.data && namespaces.data.namespaces.length > 1 ? (
          <select value={namespaceId} onChange={(e) => setNamespaceId(e.target.value)}>
            {namespaces.data.namespaces.map((ns) => (
              <option key={ns.namespace_id} value={ns.namespace_id}>
                {ns.display_name}
              </option>
            ))}
          </select>
        ) : undefined
      }
    >
      <p className="hint">
        O que casar com uma destas regras nunca é enviado para a nuvem nem baixado — fica só no disco, fora da sincronização (ex.: <code>node_modules/</code>,
        <code>vendor/</code>).
      </p>

      {suggestions.data && suggestions.data.suggestions.length > 0 && (
        <div className="ignore-suggestions">
          {suggestions.data.suggestions.map((s) => (
            <div key={s.manifest_file} className="ignore-suggestion">
              <span>
                Encontramos <code>{s.manifest_file}</code> — aplicar o perfil <strong>{s.name}</strong> ({s.patterns.join(", ")})?
              </span>
              <button onClick={() => applyProfile.run(s.manifest_file)} disabled={applyProfile.pending}>
                {applyProfile.pending ? "Aplicando…" : "Aplicar"}
              </button>
            </div>
          ))}
        </div>
      )}

      <div className="actions" style={{ marginBottom: "0.9rem" }}>
        <input value={pattern} style={{ maxWidth: "300px", width: "calc(100% - 100px)" }} onChange={(e) => setPattern(e.target.value)} placeholder="Ex.: node_modules/ ou *.log" disabled={addRule.pending} />
        <button className="btn-primary" onClick={() => addRule.run(pattern)} disabled={addRule.pending || !pattern.trim()}>
          {addRule.pending ? "Adicionando…" : "Adicionar"}
        </button>
      </div>

      {rules.error && <p className="error">{rules.error}</p>}
      <div className="table-scroll">
        <table>
          <thead>
            <tr>
              <th>Padrão</th>
              <th>Origem</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {rules.data?.rules.map((rule) => (
              <tr key={rule.rule_id}>
                <td>
                  <code>{rule.pattern}</code>
                </td>
                <td>
                  <span className="badge">{IGNORE_TIER_LABELS[rule.tier] ?? rule.tier}</span>
                </td>
                <td className="actions">
                  <button className="btn-danger" onClick={() => removeRule.run(rule.rule_id)} disabled={removeRule.pending}>
                    Remover
                  </button>
                </td>
              </tr>
            ))}
            {!rules.loading && rules.data?.rules.length === 0 && <EmptyRow colSpan={3}>Nenhuma regra de exclusão ativa.</EmptyRow>}
          </tbody>
        </table>
      </div>
    </Card>
  );
}

interface LogLine {
  id: number;
  namespace_id?: string;
  type: string;
  at: number;
  /** Título já resolvido — o quê aconteceu (ex.: "Arquivo enviado"). */
  title: string;
  /** Complemento — geralmente o caminho do item afetado. */
  detail?: string;
}

const LOG_CAP = 1000;

const LOG_LABELS: Record<string, string> = {
  NAMESPACE_MOUNTED: "Conta montada",
  NAMESPACE_UNMOUNTED: "Conta desmontada",
  CONFLICT_CREATED: "Conflito detectado",
  CONFLICT_RESOLVED: "Conflito resolvido",
  REFRESH_COMPLETED: "Atualização concluída",
  CACHE_PRESSURE_CHANGED: "Pressão de cache",
  FOLDER_LISTED: "Pasta acessada",
};

/** T5-desktop ("mostrar o que está sendo feito"): rótulo por tipo de
 * operação do journal, casado com `nexofs_domain::states::OperationType`
 * (`operation_type_to_sql`, `nexofs-sync-core/src/model.rs`). */
const OPERATION_TYPE_LABELS: Record<string, string> = {
  UPLOAD_FILE: "Arquivo enviado",
  CREATE_DIRECTORY: "Pasta criada",
  MOVE_ITEM: "Item movido",
  RENAME_ITEM: "Item renomeado",
  DELETE_ITEM: "Item excluído",
  RESTORE_ITEM: "Item restaurado",
  HYDRATE_ITEM: "Arquivo baixado",
  PIN_TREE: "Fixação aplicada",
  REFRESH_CHANGES: "Verificação de mudanças",
  RECONCILE_NAMESPACE: "Reconciliação do índice",
};

/** Estados que ainda não valem a pena logar (ficaria uma linha "Na fila"
 * para toda escrita, sem nenhuma informação nova) — só o que representa
 * progresso real de verdade aparece na aba. */
const LOGGABLE_OPERATION_STATES = new Set(["RUNNING", "COMPLETED", "FAILED_PERMANENT", "CANCELLED"]);

function describeEvent(type: string, payload: Record<string, unknown>): { title: string; detail?: string } | null {
  if (type === "FOLDER_LISTED") {
    const name = (typeof payload.name === "string" && payload.name) || "Raiz";
    return { title: LOG_LABELS.FOLDER_LISTED, detail: name };
  }
  if (type === "OPERATION_PROGRESS") {
    const state = typeof payload.state === "string" ? payload.state : "";
    if (!LOGGABLE_OPERATION_STATES.has(state)) return null;
    const operationType = typeof payload.operation_type === "string" ? payload.operation_type : undefined;
    const itemPath = typeof payload.item_path === "string" ? payload.item_path : undefined;
    const itemName = typeof payload.item_name === "string" ? payload.item_name : undefined;
    const baseTitle = (operationType && OPERATION_TYPE_LABELS[operationType]) ?? "Operação";
    const title = `${baseTitle} — ${STATE_LABELS[state] ?? state}`;
    return { title, detail: itemPath ?? itemName };
  }
  const level = typeof payload.level === "string" ? payload.level : undefined;
  return { title: LOG_LABELS[type] ?? type, detail: level };
}

/** Log "ao vivo" a partir do momento em que a janela abriu — não é um
 * histórico persistido (SPEC ainda não tem um journal de eventos
 * consultável), então a lista fica só em memória, sempre capada em
 * `LOG_CAP` linhas para nunca crescer sem limite e travar a aplicação com
 * uma conta muito ativa. */
function SyncLog() {
  const namespaces = useApiList<{ namespaces: NamespaceSummary[] }>(api.namespaces, []);
  const [filter, setFilter] = useState<string>("ALL");
  const [lines, setLines] = useState<LogLine[]>([]);
  const nextId = useRef(0);

  useEffect(() => {
    const unlisten = listen<Record<string, unknown>>("nexofs://event", (event) => {
      const payload = event.payload;
      const type = typeof payload.type === "string" ? payload.type : "EVENTO";
      const namespace_id = typeof payload.namespace_id === "string" ? payload.namespace_id : undefined;
      const described = describeEvent(type, payload);
      if (!described) return;
      setLines((current) => {
        const next = [...current, { id: nextId.current++, namespace_id, type, at: Date.now(), title: described.title, detail: described.detail }];
        return next.length > LOG_CAP ? next.slice(next.length - LOG_CAP) : next;
      });
    });
    return () => {
      unlisten.then((stop) => stop());
    };
  }, []);

  const visible = filter === "ALL" ? lines : lines.filter((line) => line.namespace_id === filter);

  return (
    <Card
      title="Log de sincronização"
      action={
        <div className="actions">
          {namespaces.data && namespaces.data.namespaces.length > 0 && (
            <select value={filter} onChange={(e) => setFilter(e.target.value)}>
              <option value="ALL">Todas as contas</option>
              {namespaces.data.namespaces.map((ns) => (
                <option key={ns.namespace_id} value={ns.namespace_id}>
                  {ns.display_name}
                </option>
              ))}
            </select>
          )}
          <button onClick={() => setLines([])} disabled={lines.length === 0}>
            Limpar log
          </button>
        </div>
      }
    >
      <p className="hint">
        Eventos a partir do momento em que esta janela foi aberta — não é um histórico retroativo. No máximo {LOG_CAP} linhas: as mais antigas são descartadas.
      </p>
      <div className="log-list">
        {visible.length === 0 && <p className="empty-row">Nenhum evento ainda.</p>}
        {visible
          .slice()
          .reverse()
          .map((line) => {
            const ns = namespaces.data?.namespaces.find((n) => n.namespace_id === line.namespace_id);
            return (
              <div key={line.id} className="log-line">
                <span className="log-time">{new Date(line.at).toLocaleTimeString("pt-BR")}</span>
                <span className="log-account">{ns?.display_name ?? "—"}</span>
                <span className="log-type">{line.title}</span>
                {line.detail && <span className="log-detail">{line.detail}</span>}
              </div>
            );
          })}
      </div>
    </Card>
  );
}

function DiagnosticsLink() {
  const [savedTo, setSavedTo] = useState<string | null>(null);
  const diagnostics = useAction(api.generateDiagnosticsPackage, { onSuccess: () => {} });

  return (
    <div className="footer-diagnostics">
      <button
        className="link-button"
        onClick={async () => {
          const result = await diagnostics.run();
          if (result) setSavedTo(result.saved_to);
        }}
        disabled={diagnostics.pending}
      >
        {diagnostics.pending ? "Gerando diagnóstico…" : "Gerar pacote de diagnóstico"}
      </button>
      {savedTo && <span className="hint"> — salvo em: {savedTo}</span>}
    </div>
  );
}

const TABS = ["Contas", "Arquivos", "Exclusões", "Operações", "Conflitos", "Cache", "Log"] as const;
type Tab = (typeof TABS)[number];

function AppContent() {
  const [filesKey, setFilesKey] = useState(0);
  const bumpFiles = () => setFilesKey((k) => k + 1);
  const [tab, setTab] = useState<Tab>("Contas");
  const [showHelp, setShowHelp] = useState(false);
  const conflicts = useApiList<{ conflicts: ConflictSummary[] }>(api.conflicts, ["CONFLICT_CREATED", "CONFLICT_RESOLVED"]);
  const conflictCount = conflicts.data?.conflicts.length ?? 0;

  return (
    <main>
      <header>
        <div className="brand">
          <img src={logoLight} alt="NexoFS" className="brand-logo brand-logo-light" />
          <img src={logoDark} alt="NexoFS" className="brand-logo brand-logo-dark" />
        </div>
        <button className="help-button" onClick={() => setShowHelp(true)} aria-label="Ajuda" title="Ajuda">
          ?
        </button>
      </header>
      {showHelp && <HelpModal onClose={() => setShowHelp(false)} />}
      <nav className="tabbar">
        {TABS.map((t) => (
          <button key={t} className={`tab ${t === tab ? "tab-active" : ""}`} onClick={() => setTab(t)}>
            {t}
            {t === "Conflitos" && conflictCount > 0 && <span className="tab-badge">{conflictCount}</span>}
          </button>
        ))}
      </nav>
      <div className="tab-panel">
        {tab === "Contas" && <AccountsAndNamespaces onAccountAdded={bumpFiles} />}
        {tab === "Arquivos" && <FilesBrowser key={filesKey} />}
        {tab === "Operações" && <OperationsQueue />}
        {tab === "Conflitos" && <Conflicts />}
        {tab === "Exclusões" && <IgnoreRulesTab />}
        {tab === "Cache" && <CacheUsage />}
        {/* Sempre montado (só escondido via CSS): ao contrário das outras
            abas, o log precisa continuar acumulando eventos em segundo
            plano — desmontar ao trocar de aba apagava tudo e reiniciava o
            listener do zero a cada volta. */}
        <div style={{ display: tab === "Log" ? undefined : "none" }}>
          <SyncLog />
        </div>
      </div>
      <footer>
        <DiagnosticsLink />
      </footer>
    </main>
  );
}

export default function App() {
  return (
    <ToastProvider>
      <AppContent />
    </ToastProvider>
  );
}
