// Espelha exatamente as formas de JSON produzidas por
// `crates/nexofs-local-api/src/routes.rs` — nenhum tipo aqui é inventado,
// são os mesmos campos que a API já devolve e que `nexofs-cli`/os testes
// de integração já exercitam.
import { invoke } from "@tauri-apps/api/core";

export const CloudProviders = [
  { id: "onedrive", label: "Microsoft OneDrive" },
  { id: "googledrive", label: "Google Drive" },
] as const;

export interface AccountSummary {
  account_id: string;
  provider_id: string;
  display_name: string;
}

export interface NamespaceSummary {
  namespace_id: string;
  account_id: string;
  display_name: string;
  mount_path: string;
  mount_state: string;
}

export interface OperationSummary {
  operation_id: string;
  namespace_id: string;
  item_id: string | null;
  item_name: string | null;
  item_path: string | null;
  operation_type: string;
  state: string;
  priority: number;
  attempt_count: number;
  last_error_message: string | null;
  updated_at: number;
}

/** Mesmos valores aceitos por `GET /v1/operations` (`?state=`) — só os
 * estados que essa rota realmente pode devolver (T7-06). */
export interface OperationsQuery {
  limit?: number;
  offset?: number;
  operationState?: string;
  operationType?: string;
  search?: string;
}

export interface ConflictSummary {
  conflict_id: string;
  item_id: string;
  item_name: string | null;
  item_path: string | null;
  conflict_type: string;
  state: string;
  detected_at: number;
}

export interface CacheEntry {
  namespace_id: string;
  hydrated_items: number;
  hydrated_bytes: number;
  clean_items: number;
  clean_bytes: number;
  dirty_items: number;
  dirty_bytes: number;
  partial_items: number;
  partial_bytes: number;
  overlay_items: number;
  overlay_bytes: number;
}

export const PinStates = ["ONLINE_ONLY", "AVAILABLE_LOCALLY", "PINNED"] as const;
export type PinState = (typeof PinStates)[number];

export interface NamespaceItem {
  item_id: string;
  name: string;
  kind: "File" | "Directory";
  size_bytes: number;
  sync_state: string | null;
  source_layer: string;
  pin_state: PinState;
}

export const ConflictResolutions = ["KEEP_LOCAL", "KEEP_REMOTE", "KEEP_BOTH", "SAVE_LOCAL_ELSEWHERE", "DISMISS_TEMPORARILY"] as const;
export type ConflictResolution = (typeof ConflictResolutions)[number];

export interface IgnoreRule {
  rule_id: string;
  tier: string;
  pattern: string;
}

export interface IgnoreProfileSuggestion {
  name: string;
  manifest_file: string;
  patterns: string[];
}

export const api = {
  status: () => invoke<{ namespaces: { namespace_id: string }[] }>("get_status"),
  accounts: () => invoke<{ accounts: AccountSummary[] }>("get_accounts"),
  addAccount: (providerId: string, mountPath?: string, displayName?: string) =>
    invoke<{ namespace: NamespaceSummary }>("add_account", { providerId, mountPath: mountPath ?? null, displayName: displayName ?? null }),
  unmountAccount: (accountId: string) => invoke("unmount_account", { accountId }),
  remountAccount: (accountId: string) => invoke<{ namespace: NamespaceSummary }>("remount_account", { accountId }),
  deleteAccount: (accountId: string) => invoke("delete_account", { accountId }),
  namespaces: () => invoke<{ namespaces: NamespaceSummary[] }>("get_namespaces"),
  items: (namespaceId: string, parentItemId?: string) =>
    invoke<{ parent_item_id: string; items: NamespaceItem[] }>("list_items", { namespaceId, parentItemId: parentItemId ?? null }),
  setPinState: (namespaceId: string, itemId: string, pinState: PinState, recursive = false) =>
    invoke("set_pin_state", { namespaceId, itemId, pinState, recursive }),
  operations: (query: OperationsQuery = {}) =>
    invoke<{ operations: OperationSummary[]; total: number; total_failed: number }>("get_operations", {
      limit: query.limit ?? null,
      offset: query.offset ?? null,
      operationState: query.operationState ?? null,
      operationType: query.operationType ?? null,
      search: query.search ?? null,
    }),
  retryOperation: (operationId: string) => invoke("retry_operation", { operationId }),
  cancelOperation: (operationId: string) => invoke("cancel_operation", { operationId }),
  conflicts: () => invoke<{ conflicts: ConflictSummary[] }>("get_conflicts"),
  resolveConflict: (conflictId: string, resolution: ConflictResolution) => invoke("resolve_conflict", { conflictId, resolution }),
  ignoreRules: (namespaceId: string) => invoke<{ rules: IgnoreRule[] }>("get_ignore_rules", { namespaceId }),
  addIgnoreRule: (namespaceId: string, pattern: string) => invoke("add_ignore_rule", { namespaceId, pattern }),
  removeIgnoreRule: (namespaceId: string, ruleId: string) => invoke("remove_ignore_rule", { namespaceId, ruleId }),
  ignoreProfileSuggestions: (namespaceId: string) =>
    invoke<{ suggestions: IgnoreProfileSuggestion[] }>("ignore_profile_suggestions", { namespaceId }),
  applyIgnoreProfile: (namespaceId: string, manifestFile: string) => invoke("apply_ignore_profile", { namespaceId, manifestFile }),
  cache: () => invoke<{ cache: CacheEntry[]; max_bytes_per_namespace: number }>("get_cache"),
  cleanupCache: () => invoke("cleanup_cache"),
  generateDiagnosticsPackage: () => invoke<{ saved_to: string }>("generate_diagnostics_package"),
  refreshNamespace: (namespaceId: string) => invoke("refresh_namespace", { namespaceId }),
  syncNow: (namespaceId: string) => invoke("sync_now", { namespaceId }),
};
