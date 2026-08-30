//! Endpoints mínimos da API local. SPEC §20.3 — apenas o subconjunto de
//! leitura de status e a atualização manual (T2-09); o restante do
//! catálogo de endpoints chega com a UI/CLI na Fase 5 (T5-01).

use crate::state::AppState;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use nexofs_domain::states::{OperationState, OperationType};
use nexofs_domain::{AccountId, NamespaceId, OperationId};
use serde::Serialize;
use serde_json::{json, Value};
use std::convert::Infallible;
use tokio_stream::StreamExt;

#[derive(Serialize)]
struct NamespaceStatus {
    namespace_id: String,
}

#[derive(Serialize)]
struct StatusResponse {
    namespaces: Vec<NamespaceStatus>,
}

async fn get_status(State(state): State<AppState>) -> Json<StatusResponse> {
    let namespaces = state
        .mounted
        .read()
        .await
        .namespaces
        .keys()
        .map(|id| NamespaceStatus { namespace_id: id.to_string() })
        .collect();
    Json(StatusResponse { namespaces })
}

/// `POST /v1/namespaces/{id}/refresh` (SPEC §14.4, FR-REF-001 a 006):
/// prioridade alta, idempotente enquanto já em voo, respeita o Governor
/// (por dentro de `SyncCore::refresh_changes`), nunca força full scan.
async fn post_refresh(
    State(state): State<AppState>,
    Path(namespace_id_raw): Path<String>,
) -> (StatusCode, Json<Value>) {
    let Ok(uuid) = uuid::Uuid::parse_str(&namespace_id_raw) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": {"code": "INVALID_ID", "message": "namespace_id inválido"}})),
        );
    };
    let namespace_id = NamespaceId(uuid);

    let Some(sync_core) = state.sync_core_for(namespace_id).await else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": {"code": "NOT_FOUND", "message": "namespace desconhecido ou não montado"}})),
        );
    };

    let success = state
        .refresh_dedup
        .run(namespace_id, move || async move { sync_core.refresh_changes().await.is_ok() })
        .await;

    if success {
        (StatusCode::OK, Json(json!({"status": "ok"})))
    } else {
        (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": {"code": "RATE_LIMITED", "message": "O provedor limitou temporariamente novas operações ou a chamada falhou. Alterações locais estão seguras."}})),
        )
    }
}

/// `POST /v1/namespaces/{id}/sync-now` (T3-04/SPEC §16.2, quarto gatilho de
/// estabilização — "comando manual"): estabiliza todo item ainda `Dirty`
/// deste namespace e já dispara uma rodada de despacho, em vez de esperar o
/// debounce de 5s ou o tick periódico do daemon. Idempotente enquanto já em
/// voo, mesmo padrão de dedup de `post_refresh`.
async fn post_sync_now(
    State(state): State<AppState>,
    Path(namespace_id_raw): Path<String>,
) -> (StatusCode, Json<Value>) {
    let Ok(uuid) = uuid::Uuid::parse_str(&namespace_id_raw) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": {"code": "INVALID_ID", "message": "namespace_id inválido"}})),
        );
    };
    let namespace_id = NamespaceId(uuid);

    let Some(sync_core) = state.sync_core_for(namespace_id).await else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": {"code": "NOT_FOUND", "message": "namespace desconhecido ou não montado"}})),
        );
    };

    let success = state
        .sync_now_dedup
        .run(namespace_id, move || async move {
            if sync_core.stabilize_all_dirty_items().await.is_err() {
                return false;
            }
            sync_core.dispatch_pending_operations().await.is_ok()
        })
        .await;

    if success {
        (StatusCode::OK, Json(json!({"status": "ok"})))
    } else {
        (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": {"code": "RATE_LIMITED", "message": "O provedor limitou temporariamente novas operações ou a chamada falhou. Alterações locais estão seguras."}})),
        )
    }
}

/// `GET /v1/namespaces/{id}/ignore-rules` (T4-03): regras de exclusão hoje
/// ativas neste namespace, com a camada de cada uma — a origem que a UI/CLI
/// precisa mostrar (SPEC §17.2/§17.3).
async fn get_ignore_rules(State(state): State<AppState>, Path(namespace_id_raw): Path<String>) -> (StatusCode, Json<Value>) {
    let Ok(uuid) = uuid::Uuid::parse_str(&namespace_id_raw) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": {"code": "INVALID_ID", "message": "namespace_id inválido"}})));
    };
    let Some(sync_core) = state.sync_core_for(NamespaceId(uuid)).await else {
        return (StatusCode::NOT_FOUND, Json(json!({"error": {"code": "NOT_FOUND", "message": "namespace desconhecido ou não montado"}})));
    };

    match sync_core.list_ignore_rules_with_ids().await {
        Ok(rules) => {
            let rules: Vec<Value> = rules
                .into_iter()
                .map(|(rule_id, tier, pattern)| json!({"rule_id": rule_id, "tier": format!("{:?}", tier), "pattern": pattern}))
                .collect();
            (StatusCode::OK, Json(json!({ "rules": rules })))
        }
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": {"code": "INTERNAL", "message": err.to_string()}}))),
    }
}

#[derive(serde::Deserialize)]
struct AddIgnoreRuleRequest {
    pattern: String,
}

/// `POST /v1/namespaces/{id}/ignore-rules` (T5-06, "criar"): sempre grava
/// como `RuleTier::Account` — a UI não expõe as 8 camadas de `SPEC §17.2`
/// (isso exigiria uma tela própria de administração de política, fora do
/// escopo desta tela), só uma exclusão de propósito geral por conta.
async fn post_add_ignore_rule(
    State(state): State<AppState>,
    Path(namespace_id_raw): Path<String>,
    Json(body): Json<AddIgnoreRuleRequest>,
) -> (StatusCode, Json<Value>) {
    let Ok(uuid) = uuid::Uuid::parse_str(&namespace_id_raw) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": {"code": "INVALID_ID", "message": "namespace_id inválido"}})));
    };
    let Some(sync_core) = state.sync_core_for(NamespaceId(uuid)).await else {
        return (StatusCode::NOT_FOUND, Json(json!({"error": {"code": "NOT_FOUND", "message": "namespace desconhecido ou não montado"}})));
    };
    if body.pattern.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": {"code": "INVALID_PATTERN", "message": "padrão não pode ser vazio"}})));
    }

    match sync_core.add_ignore_rule(nexofs_sync_core::RuleTier::Account, body.pattern.trim()).await {
        Ok(()) => (StatusCode::OK, Json(json!({"status": "ok"}))),
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": {"code": "INTERNAL", "message": err.to_string()}}))),
    }
}

/// `DELETE /v1/namespaces/{id}/ignore-rules/{rule_id}` (T5-06, "remover").
async fn delete_ignore_rule(State(state): State<AppState>, Path((namespace_id_raw, rule_id)): Path<(String, String)>) -> (StatusCode, Json<Value>) {
    let Ok(uuid) = uuid::Uuid::parse_str(&namespace_id_raw) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": {"code": "INVALID_ID", "message": "namespace_id inválido"}})));
    };
    let Some(sync_core) = state.sync_core_for(NamespaceId(uuid)).await else {
        return (StatusCode::NOT_FOUND, Json(json!({"error": {"code": "NOT_FOUND", "message": "namespace desconhecido ou não montado"}})));
    };

    match sync_core.remove_ignore_rule(&rule_id).await {
        Ok(true) => (StatusCode::OK, Json(json!({"status": "ok"}))),
        Ok(false) => (StatusCode::NOT_FOUND, Json(json!({"error": {"code": "NOT_FOUND", "message": "regra não encontrada neste namespace"}}))),
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": {"code": "INTERNAL", "message": err.to_string()}}))),
    }
}

/// `GET /v1/namespaces/{id}/ignore-profiles/suggestions` (T4-04): perfis de
/// tecnologia sugeridos a partir de manifestos na raiz — nunca aplicados
/// sozinhos (SPEC §17.4).
async fn get_ignore_profile_suggestions(State(state): State<AppState>, Path(namespace_id_raw): Path<String>) -> (StatusCode, Json<Value>) {
    let Ok(uuid) = uuid::Uuid::parse_str(&namespace_id_raw) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": {"code": "INVALID_ID", "message": "namespace_id inválido"}})));
    };
    let Some(sync_core) = state.sync_core_for(NamespaceId(uuid)).await else {
        return (StatusCode::NOT_FOUND, Json(json!({"error": {"code": "NOT_FOUND", "message": "namespace desconhecido ou não montado"}})));
    };

    match sync_core.suggest_ignore_profiles().await {
        Ok(profiles) => {
            let profiles: Vec<Value> = profiles
                .into_iter()
                .map(|profile| json!({"name": profile.name, "manifest_file": profile.manifest_file, "patterns": profile.patterns}))
                .collect();
            (StatusCode::OK, Json(json!({ "suggestions": profiles })))
        }
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": {"code": "INTERNAL", "message": err.to_string()}}))),
    }
}

#[derive(serde::Deserialize)]
struct ApplyProfileRequest {
    manifest_file: String,
}

/// `POST /v1/namespaces/{id}/ignore-profiles/apply` (T4-04): a confirmação
/// explícita exigida por SPEC §17.4 antes de um perfil sugerido virar regra
/// de verdade. Identificado por `manifest_file` (não por `name` — Python tem
/// dois manifestos possíveis para o mesmo perfil).
async fn post_apply_ignore_profile(
    State(state): State<AppState>,
    Path(namespace_id_raw): Path<String>,
    Json(body): Json<ApplyProfileRequest>,
) -> (StatusCode, Json<Value>) {
    let Ok(uuid) = uuid::Uuid::parse_str(&namespace_id_raw) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": {"code": "INVALID_ID", "message": "namespace_id inválido"}})));
    };
    let Some(sync_core) = state.sync_core_for(NamespaceId(uuid)).await else {
        return (StatusCode::NOT_FOUND, Json(json!({"error": {"code": "NOT_FOUND", "message": "namespace desconhecido ou não montado"}})));
    };
    let Some(profile) = nexofs_sync_core::KNOWN_PROFILES.iter().find(|p| p.manifest_file == body.manifest_file) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": {"code": "UNKNOWN_PROFILE", "message": "manifest_file não corresponde a nenhum perfil conhecido"}})));
    };

    match sync_core.apply_ignore_profile(profile).await {
        Ok(()) => (StatusCode::OK, Json(json!({"status": "ok"}))),
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": {"code": "INTERNAL", "message": err.to_string()}}))),
    }
}

/// `GET /v1/namespaces/{id}/storm-paused-folders` (T4-09/SPEC §7.9): pastas
/// hoje pausadas por tempestade de arquivos, aguardando revisão.
async fn get_storm_paused_folders(State(state): State<AppState>, Path(namespace_id_raw): Path<String>) -> (StatusCode, Json<Value>) {
    let Ok(uuid) = uuid::Uuid::parse_str(&namespace_id_raw) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": {"code": "INVALID_ID", "message": "namespace_id inválido"}})));
    };
    let Some(sync_core) = state.sync_core_for(NamespaceId(uuid)).await else {
        return (StatusCode::NOT_FOUND, Json(json!({"error": {"code": "NOT_FOUND", "message": "namespace desconhecido ou não montado"}})));
    };
    let folders: Vec<String> = sync_core.storm_paused_folders().into_iter().map(|id| id.to_string()).collect();
    (StatusCode::OK, Json(json!({ "paused_folders": folders })))
}

#[derive(serde::Deserialize)]
struct ResumeStormRequest {
    folder_item_id: String,
}

/// `POST /v1/namespaces/{id}/storm-resume` (T4-09): retomada explícita —
/// nunca automática — depois de revisar a classificação de risco.
async fn post_resume_storm(
    State(state): State<AppState>,
    Path(namespace_id_raw): Path<String>,
    Json(body): Json<ResumeStormRequest>,
) -> (StatusCode, Json<Value>) {
    let Ok(uuid) = uuid::Uuid::parse_str(&namespace_id_raw) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": {"code": "INVALID_ID", "message": "namespace_id inválido"}})));
    };
    let Some(sync_core) = state.sync_core_for(NamespaceId(uuid)).await else {
        return (StatusCode::NOT_FOUND, Json(json!({"error": {"code": "NOT_FOUND", "message": "namespace desconhecido ou não montado"}})));
    };
    let Ok(folder_uuid) = uuid::Uuid::parse_str(&body.folder_item_id) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": {"code": "INVALID_ID", "message": "folder_item_id inválido"}})));
    };

    match sync_core.resume_from_storm_pause(nexofs_domain::ItemId(folder_uuid)).await {
        Ok(stabilized) => (StatusCode::OK, Json(json!({"status": "ok", "stabilized_items": stabilized}))),
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": {"code": "INTERNAL", "message": err.to_string()}}))),
    }
}

#[derive(serde::Deserialize)]
struct SetPinStateRequest {
    item_id: String,
    pin_state: String,
    recursive: Option<bool>,
}

/// `POST /v1/namespaces/{id}/pin` (T4-10/FR-PIN-001/002): aplica um estado
/// de fixação a um item; `recursive: true` também fixa toda a subárvore e
/// hidrata os descendentes em segundo plano, sem bloquear esta resposta.
async fn post_set_pin_state(
    State(state): State<AppState>,
    Path(namespace_id_raw): Path<String>,
    Json(body): Json<SetPinStateRequest>,
) -> (StatusCode, Json<Value>) {
    let Ok(uuid) = uuid::Uuid::parse_str(&namespace_id_raw) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": {"code": "INVALID_ID", "message": "namespace_id inválido"}})));
    };
    let Some(sync_core) = state.sync_core_for(NamespaceId(uuid)).await else {
        return (StatusCode::NOT_FOUND, Json(json!({"error": {"code": "NOT_FOUND", "message": "namespace desconhecido ou não montado"}})));
    };
    let Ok(item_uuid) = uuid::Uuid::parse_str(&body.item_id) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": {"code": "INVALID_ID", "message": "item_id inválido"}})));
    };
    let item_id = nexofs_domain::ItemId(item_uuid);
    let pin_state = match body.pin_state.as_str() {
        "ONLINE_ONLY" => nexofs_sync_core::PinState::OnlineOnly,
        "AVAILABLE_LOCALLY" => nexofs_sync_core::PinState::AvailableLocally,
        "PINNED" => nexofs_sync_core::PinState::Pinned,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": {"code": "INVALID_PIN_STATE", "message": "pin_state deve ser ONLINE_ONLY, AVAILABLE_LOCALLY ou PINNED"}})),
            )
        }
    };

    if body.recursive.unwrap_or(false) && pin_state == nexofs_sync_core::PinState::Pinned {
        sync_core.pin_recursive(item_id);
        return (StatusCode::OK, Json(json!({"status": "ok", "recursive": true})));
    }

    match sync_core.set_pin_state(item_id, pin_state).await {
        Ok(()) => (StatusCode::OK, Json(json!({"status": "ok"}))),
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": {"code": "INTERNAL", "message": err.to_string()}}))),
    }
}

fn pin_state_label(pin_state: nexofs_sync_core::PinState) -> &'static str {
    match pin_state {
        nexofs_sync_core::PinState::OnlineOnly => "ONLINE_ONLY",
        nexofs_sync_core::PinState::AvailableLocally => "AVAILABLE_LOCALLY",
        nexofs_sync_core::PinState::Pinned => "PINNED",
    }
}

#[derive(serde::Deserialize)]
struct ListItemsQuery {
    parent_item_id: Option<String>,
}

/// `GET /v1/namespaces/{id}/items?parent_item_id=...` (T5-desktop —
/// navegador de árvore para a fixação seletiva, FR-PIN-001/002): filhos de
/// `parent_item_id`, ou da raiz do namespace quando omitido. Cada item já
/// vem com o `pin_state` resolvido — a tela não precisa de uma segunda
/// chamada por item para desenhar a marca de "mantido localmente".
async fn get_namespace_items(
    State(state): State<AppState>,
    Path(namespace_id_raw): Path<String>,
    Query(query): Query<ListItemsQuery>,
) -> (StatusCode, Json<Value>) {
    let Ok(uuid) = uuid::Uuid::parse_str(&namespace_id_raw) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": {"code": "INVALID_ID", "message": "namespace_id inválido"}})));
    };
    let Some(sync_core) = state.sync_core_for(NamespaceId(uuid)).await else {
        return (StatusCode::NOT_FOUND, Json(json!({"error": {"code": "NOT_FOUND", "message": "namespace desconhecido ou não montado"}})));
    };

    let parent_item_id = match query.parent_item_id {
        Some(raw) => match uuid::Uuid::parse_str(&raw) {
            Ok(uuid) => nexofs_domain::ItemId(uuid),
            Err(_) => return (StatusCode::BAD_REQUEST, Json(json!({"error": {"code": "INVALID_ID", "message": "parent_item_id inválido"}}))),
        },
        None => match sync_core.bootstrap_root().await {
            Ok(root) => root,
            Err(err) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": {"code": "INTERNAL", "message": err.to_string()}}))),
        },
    };

    let children = match sync_core.list_children(parent_item_id).await {
        Ok(children) => children,
        Err(err) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": {"code": "INTERNAL", "message": err.to_string()}}))),
    };

    let mut items: Vec<Value> = Vec::with_capacity(children.len());
    for child in children {
        let pin_state = match sync_core.pin_state_of(child.item_id).await {
            Ok(state) => state,
            Err(err) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": {"code": "INTERNAL", "message": err.to_string()}}))),
        };
        items.push(json!({
            "item_id": child.item_id.to_string(),
            "name": child.name,
            "kind": format!("{:?}", child.kind),
            "size_bytes": child.size_bytes,
            "sync_state": child.sync_state,
            "source_layer": child.source_layer,
            "pin_state": pin_state_label(pin_state),
        }));
    }

    (StatusCode::OK, Json(json!({ "parent_item_id": parent_item_id.to_string(), "items": items })))
}

/// Resolve nome/caminho do item envolvido para a UI mostrar qual arquivo
/// está em conflito em vez de só o `item_id` (SPEC §20.3 não exige, mas sem
/// isso a tela de conflitos era inútil na prática — usuário não tem como
/// saber o que resolver). `None` quando o item já não existe mais no índice
/// (ex: apagado antes do conflito ser resolvido) — não é erro.
async fn conflict_to_json(
    namespace_id: &NamespaceId,
    sync_core: &nexofs_sync_core::SyncCore,
    c: nexofs_sync_core::ConflictSummary,
    item_cache: &mut std::collections::HashMap<nexofs_domain::ItemId, nexofs_sync_core::IndexedItem>,
) -> Value {
    let item_path = sync_core.item_relative_path_cached(c.item_id, item_cache).await.ok().map(|p| p.to_string_lossy().to_string());
    let item_name = item_cache.get(&c.item_id).map(|item| item.name.clone());
    json!({
        "conflict_id": c.conflict_id.to_string(),
        "namespace_id": namespace_id.to_string(),
        "item_id": c.item_id.to_string(),
        "item_name": item_name,
        "item_path": item_path,
        "conflict_type": serde_json::to_value(c.conflict_type).unwrap(),
        "state": c.state,
        "detected_at": c.detected_at,
    })
}

/// `GET /v1/conflicts` (SPEC §20.3): conflitos abertos agregados de todos
/// os namespaces montados — mesmo padrão de `get_operations`/`get_cache`.
/// A UI/CLI chamam este, não o namespace-scoped abaixo (que continua
/// existindo para quem já sabe o namespace de antemão).
async fn get_all_conflicts(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
    let mut conflicts: Vec<Value> = Vec::new();
    for (namespace_id, sync_core) in state.namespaces_snapshot().await.iter() {
        // Mesmo cache de `get_operations` (ver `item_relative_path_cached`)
        // — hoje o volume de conflitos abertos é baixo, mas nada impede
        // várias colisões sob as mesmas pastas de acontecerem de uma vez.
        let mut item_cache = std::collections::HashMap::new();
        match sync_core.list_conflicts().await {
            Ok(namespace_conflicts) => {
                for c in namespace_conflicts {
                    conflicts.push(conflict_to_json(namespace_id, sync_core, c, &mut item_cache).await);
                }
            }
            Err(err) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": {"code": "INTERNAL", "message": err.to_string()}}))),
        }
    }
    (StatusCode::OK, Json(json!({ "conflicts": conflicts })))
}

/// `GET /v1/namespaces/{id}/conflicts` (T4-03/T4-12): conflitos ainda
/// abertos deste namespace.
async fn get_conflicts(State(state): State<AppState>, Path(namespace_id_raw): Path<String>) -> (StatusCode, Json<Value>) {
    let Ok(uuid) = uuid::Uuid::parse_str(&namespace_id_raw) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": {"code": "INVALID_ID", "message": "namespace_id inválido"}})));
    };
    let namespace_id = NamespaceId(uuid);
    let Some(sync_core) = state.sync_core_for(namespace_id).await else {
        return (StatusCode::NOT_FOUND, Json(json!({"error": {"code": "NOT_FOUND", "message": "namespace desconhecido ou não montado"}})));
    };

    match sync_core.list_conflicts().await {
        Ok(namespace_conflicts) => {
            let mut conflicts: Vec<Value> = Vec::new();
            let mut item_cache = std::collections::HashMap::new();
            for c in namespace_conflicts {
                conflicts.push(conflict_to_json(&namespace_id, &sync_core, c, &mut item_cache).await);
            }
            (StatusCode::OK, Json(json!({ "conflicts": conflicts })))
        }
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": {"code": "INTERNAL", "message": err.to_string()}}))),
    }
}

#[derive(serde::Deserialize)]
struct ResolveConflictRequest {
    resolution: String,
}

/// `POST /v1/conflicts/{id}/resolve` (T4-12/SPEC §20.3): aplica uma
/// resolução completa a um conflito aberto.
async fn post_resolve_conflict(
    State(state): State<AppState>,
    Path(conflict_id_raw): Path<String>,
    Json(body): Json<ResolveConflictRequest>,
) -> (StatusCode, Json<Value>) {
    let Ok(conflict_uuid) = uuid::Uuid::parse_str(&conflict_id_raw) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": {"code": "INVALID_ID", "message": "conflict_id inválido"}})));
    };
    let conflict_id = nexofs_sync_core::ConflictId(conflict_uuid);
    let resolution = match body.resolution.as_str() {
        "KEEP_LOCAL" => nexofs_sync_core::ConflictResolution::KeepLocal,
        "KEEP_REMOTE" => nexofs_sync_core::ConflictResolution::KeepRemote,
        "KEEP_BOTH" => nexofs_sync_core::ConflictResolution::KeepBoth,
        "SAVE_LOCAL_ELSEWHERE" => nexofs_sync_core::ConflictResolution::SaveLocalElsewhere,
        "DISMISS_TEMPORARILY" => nexofs_sync_core::ConflictResolution::DismissTemporarily,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": {"code": "INVALID_RESOLUTION", "message": "resolution deve ser KEEP_LOCAL, KEEP_REMOTE, KEEP_BOTH, SAVE_LOCAL_ELSEWHERE ou DISMISS_TEMPORARILY"}})),
            )
        }
    };

    // O conflito pode estar em qualquer namespace montado — procura em
    // todos, já que este endpoint (SPEC §20.3) é endereçado só pelo
    // `conflict_id`, sem `namespace_id` na URL.
    for sync_core in state.namespaces_snapshot().await.values() {
        match sync_core.resolve_conflict(conflict_id, resolution).await {
            Ok(()) => return (StatusCode::OK, Json(json!({"status": "ok"}))),
            Err(nexofs_sync_core::SyncError::NotFound) => continue,
            Err(err) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": {"code": "INTERNAL", "message": err.to_string()}}))),
        }
    }
    (StatusCode::NOT_FOUND, Json(json!({"error": {"code": "NOT_FOUND", "message": "conflito não encontrado em nenhum namespace montado"}})))
}

/// `GET /v1/metrics` (FR-API-006): chamadas em voo e estado do circuit
/// breaker por escopo já visto pelo Governor, mais uso agregado de cache
/// por namespace montado — o suficiente para diagnóstico sem uma UI ainda.
async fn get_metrics(State(state): State<AppState>) -> Json<Value> {
    let scopes: Vec<Value> = state
        .governor
        .snapshot()
        .into_iter()
        .map(|m| {
            json!({
                "provider_id": m.scope.provider_id.as_ref(),
                "account_id": m.scope.account_id.to_string(),
                "namespace_id": m.scope.namespace_id.map(|id| id.to_string()),
                "operation_class": format!("{:?}", m.scope.operation_class),
                "in_flight": m.in_flight,
                "circuit_state": format!("{:?}", m.circuit_state),
            })
        })
        .collect();

    let mut cache: Vec<Value> = Vec::new();
    let mut journal: Vec<Value> = Vec::new();
    let mut connectivity: Vec<Value> = Vec::new();
    let mut disk_pressure: Vec<Value> = Vec::new();
    for (namespace_id, sync_core) in state.namespaces_snapshot().await.iter() {
        // T3-09/FR-OFF-005: sinal explícito de "offline" por namespace —
        // antes só era visível indiretamente por operações presas em retry.
        connectivity.push(json!({
            "namespace_id": namespace_id.to_string(),
            "online": sync_core.is_online().await,
        }));
        // T4-14/SPEC §19.4 item 5: "notificar o usuário" — o nível fica
        // visível aqui; a notificação de UI de verdade é Fase 5.
        if let Ok(level) = sync_core.disk_pressure() {
            disk_pressure.push(json!({
                "namespace_id": namespace_id.to_string(),
                "level": format!("{:?}", level),
            }));
        }
        if let Ok(stats) = sync_core.cache_stats().await {
            cache.push(json!({
                "namespace_id": namespace_id.to_string(),
                "hydrated_items": stats.hydrated_items,
                "hydrated_bytes": stats.hydrated_bytes,
            }));
        }
        // T3-06: visibilidade mínima do journal — quantas operações ainda
        // não foram confirmadas no remoto, por estado, sem expor payload
        // (que pode conter nomes de arquivo).
        if let Ok(pending) = sync_core.pending_operations().await {
            let mut by_state: std::collections::HashMap<&'static str, u64> = std::collections::HashMap::new();
            for op in &pending {
                *by_state.entry(match op.state {
                    nexofs_domain::states::OperationState::Pending => "PENDING",
                    nexofs_domain::states::OperationState::WaitingRetry => "WAITING_RETRY",
                    nexofs_domain::states::OperationState::WaitingNetwork => "WAITING_NETWORK",
                    nexofs_domain::states::OperationState::WaitingAuthentication => "WAITING_AUTHENTICATION",
                    _ => "OUTRO",
                }).or_insert(0) += 1;
            }
            // T7-04: `FAILED_PERMANENT` não vem de `pending_operations()` (não
            // é mais "trabalho em andamento"), mas contava aqui como zero
            // silenciosamente — a própria doc deste endpoint prometia
            // "visível em /v1/metrics" e não cumpria. Sem isto, um erro real
            // do provedor (ex.: HTTP 411) que já desistiu de vez some do
            // journal inteiro, inclusive daqui.
            let failed_permanent = sync_core.failed_operations().await.map(|ops| ops.len()).unwrap_or(0);
            if failed_permanent > 0 {
                by_state.insert("FAILED_PERMANENT", failed_permanent as u64);
            }
            journal.push(json!({
                "namespace_id": namespace_id.to_string(),
                "total": pending.len() + failed_permanent,
                "by_state": by_state,
            }));
        }
    }

    Json(json!({ "scopes": scopes, "cache": cache, "journal": journal, "connectivity": connectivity, "disk_pressure": disk_pressure }))
}

/// `POST /v1/accounts/auth/start` (SPEC §20.3): login interativo completo
/// (abre o navegador, espera o redirect OAuth, monta o FUSE da conta nova)
/// — um único passo em vez de `start`/`complete` separados, porque quem
/// sabe autenticar e montar é `nexofsd::main` (ADR-005), não esta API; o
/// handler só encaminha o pedido e espera o resultado. Fica aberta durante
/// todo o fluxo do navegador (minutos, potencialmente) — aceitável para uma
/// ação administrativa explícita, não um endpoint de alta frequência.
#[derive(serde::Deserialize)]
struct AddAccountBody {
    /// T7-02: `"onedrive"` (padrão, para nunca quebrar um chamador antigo
    /// que não sabia de outro provedor) ou `"googledrive"`.
    #[serde(default = "default_provider_id")]
    provider_id: String,
    /// Local onde montar o namespace novo — quando omitido, o daemon usa o
    /// padrão `$HOME/NexoFS/<nome>`.
    mount_path: Option<String>,
    /// Nome de exibição da montagem/namespace — quando omitido, usa o nome
    /// que o provedor devolve para a conta.
    display_name: Option<String>,
}

fn default_provider_id() -> String {
    "onedrive".to_string()
}

impl Default for AddAccountBody {
    fn default() -> Self {
        Self { provider_id: default_provider_id(), mount_path: None, display_name: None }
    }
}

/// Corpo opcional de propósito (ver `nexofs_api_client::ApiClient::post`,
/// que não manda `Content-Type` nenhum quando `body` é `None`) — por isso
/// `axum::body::Bytes` em vez de `Json<AddAccountBody>`: o extractor `Json`
/// rejeitaria um corpo vazio sem esse cabeçalho, o que quebraria todo
/// chamador que ainda não escolhe local/nome (ex.: `nexofs-cli`).
async fn post_accounts_auth_start(State(state): State<AppState>, body: axum::body::Bytes) -> (StatusCode, Json<Value>) {
    let Some(tx) = &state.add_account_tx else {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": {"code": "NOT_SUPPORTED", "message": "esta instância da API local não sabe adicionar contas dinamicamente"}})),
        );
    };
    let parsed: AddAccountBody = if body.is_empty() {
        AddAccountBody::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(parsed) => parsed,
            Err(err) => return (StatusCode::BAD_REQUEST, Json(json!({"error": {"code": "INVALID_BODY", "message": err.to_string()}}))),
        }
    };
    let (respond_to, rx) = tokio::sync::oneshot::channel();
    let request = crate::state::AddAccountRequest {
        provider_id: parsed.provider_id,
        mount_path: parsed.mount_path.map(std::path::PathBuf::from),
        display_name: parsed.display_name,
        respond_to,
    };
    if tx.send(request).await.is_err() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": {"code": "INTERNAL", "message": "o processo que monta contas não está mais respondendo"}})),
        );
    }
    match tokio::time::timeout(std::time::Duration::from_secs(300), rx).await {
        Ok(Ok(Ok(namespace))) => (StatusCode::OK, Json(json!({ "namespace": namespace }))),
        Ok(Ok(Err(message))) => (StatusCode::BAD_GATEWAY, Json(json!({"error": {"code": "AUTH_FAILED", "message": message}}))),
        Ok(Err(_)) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": {"code": "INTERNAL", "message": "resposta perdida ao adicionar conta"}}))),
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            Json(json!({"error": {"code": "TIMEOUT", "message": "login não foi concluído a tempo no navegador (5 min)"}})),
        ),
    }
}

/// `GET /v1/accounts` (SPEC §20.3): contas montadas neste daemon — nunca
/// inclui token/refresh token.
async fn get_accounts(State(state): State<AppState>) -> Json<Value> {
    Json(json!({ "accounts": state.mounted.read().await.accounts }))
}

/// `GET /v1/namespaces` (SPEC §20.3).
async fn get_namespaces(State(state): State<AppState>) -> Json<Value> {
    Json(json!({ "namespaces": state.mounted.read().await.namespace_summaries }))
}

fn parse_account_id(raw: &str) -> Result<AccountId, (StatusCode, Json<Value>)> {
    uuid::Uuid::parse_str(raw)
        .map(AccountId)
        .map_err(|_| (StatusCode::BAD_REQUEST, Json(json!({"error": {"code": "INVALID_ID", "message": "account_id inválido"}}))))
}

/// `POST /v1/accounts/{id}/unmount` (T5-desktop): desfaz a montagem FUSE
/// mas mantém a conta e o refresh token — ao contrário de excluir, dá para
/// remontar depois sem repetir login (SPEC §8 "o usuário PODE desmontar
/// temporariamente").
async fn post_unmount_account(State(state): State<AppState>, Path(account_id_raw): Path<String>) -> (StatusCode, Json<Value>) {
    let account_id = match parse_account_id(&account_id_raw) {
        Ok(id) => id,
        Err(response) => return response,
    };
    let Some(tx) = &state.account_control_tx else {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": {"code": "NOT_SUPPORTED", "message": "esta instância da API local não sabe desmontar contas dinamicamente"}})),
        );
    };
    let (respond_to, rx) = tokio::sync::oneshot::channel();
    if tx.send(crate::state::AccountControlRequest::Unmount { account_id, respond_to }).await.is_err() {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": {"code": "INTERNAL", "message": "o processo que gerencia contas não está mais respondendo"}})));
    }
    match rx.await {
        Ok(Ok(())) => (StatusCode::OK, Json(json!({"status": "ok"}))),
        Ok(Err(message)) => (StatusCode::BAD_REQUEST, Json(json!({"error": {"code": "UNMOUNT_FAILED", "message": message}}))),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": {"code": "INTERNAL", "message": "resposta perdida ao desmontar conta"}}))),
    }
}

/// `POST /v1/accounts/{id}/remount` (T5-desktop): retoma a sessão via
/// refresh token guardado — sem abrir navegador, ao contrário de
/// `POST /v1/accounts/auth/start`.
async fn post_remount_account(State(state): State<AppState>, Path(account_id_raw): Path<String>) -> (StatusCode, Json<Value>) {
    let account_id = match parse_account_id(&account_id_raw) {
        Ok(id) => id,
        Err(response) => return response,
    };
    let Some(tx) = &state.account_control_tx else {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": {"code": "NOT_SUPPORTED", "message": "esta instância da API local não sabe remontar contas dinamicamente"}})),
        );
    };
    let (respond_to, rx) = tokio::sync::oneshot::channel();
    if tx.send(crate::state::AccountControlRequest::Remount { account_id, respond_to }).await.is_err() {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": {"code": "INTERNAL", "message": "o processo que gerencia contas não está mais respondendo"}})));
    }
    match tokio::time::timeout(std::time::Duration::from_secs(30), rx).await {
        Ok(Ok(Ok(namespace))) => (StatusCode::OK, Json(json!({ "namespace": namespace }))),
        Ok(Ok(Err(message))) => (StatusCode::BAD_GATEWAY, Json(json!({"error": {"code": "REMOUNT_FAILED", "message": message}}))),
        Ok(Err(_)) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": {"code": "INTERNAL", "message": "resposta perdida ao remontar conta"}}))),
        Err(_) => (StatusCode::GATEWAY_TIMEOUT, Json(json!({"error": {"code": "TIMEOUT", "message": "remontagem demorou demais"}}))),
    }
}

/// `DELETE /v1/accounts/{id}` (T5-desktop, NFR-SEC-007): desmonta (se
/// preciso), apaga o refresh token e todo o índice local desta conta. Nunca
/// apaga os arquivos de verdade em `mount_path` — só o "conhecimento" do
/// NexoFS sobre a conta.
async fn delete_account(State(state): State<AppState>, Path(account_id_raw): Path<String>) -> (StatusCode, Json<Value>) {
    let account_id = match parse_account_id(&account_id_raw) {
        Ok(id) => id,
        Err(response) => return response,
    };
    let Some(tx) = &state.account_control_tx else {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": {"code": "NOT_SUPPORTED", "message": "esta instância da API local não sabe excluir contas dinamicamente"}})),
        );
    };
    let (respond_to, rx) = tokio::sync::oneshot::channel();
    if tx.send(crate::state::AccountControlRequest::Delete { account_id, respond_to }).await.is_err() {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": {"code": "INTERNAL", "message": "o processo que gerencia contas não está mais respondendo"}})));
    }
    match rx.await {
        Ok(Ok(())) => (StatusCode::OK, Json(json!({"status": "ok"}))),
        Ok(Err(message)) => (StatusCode::BAD_REQUEST, Json(json!({"error": {"code": "DELETE_FAILED", "message": message}}))),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": {"code": "INTERNAL", "message": "resposta perdida ao excluir conta"}}))),
    }
}

/// Resolve nome/caminho do item envolvido — mesmo motivo de
/// `conflict_to_json`: sem isto a tela de operações só mostrava tipo/estado,
/// nunca qual arquivo, e ficava impossível saber qual upload real estava
/// preso (T7-05). `None` quando o item já não existe mais no índice.
async fn operation_to_json(
    namespace_id: &NamespaceId,
    sync_core: &nexofs_sync_core::SyncCore,
    op: nexofs_sync_core::QueuedOperation,
    item_cache: &mut std::collections::HashMap<nexofs_domain::ItemId, nexofs_sync_core::IndexedItem>,
) -> Value {
    let (item_name, item_path) = match op.item_id {
        Some(item_id) => {
            let path = sync_core.item_relative_path_cached(item_id, item_cache).await.ok().map(|p| p.to_string_lossy().to_string());
            let name = item_cache.get(&item_id).map(|item| item.name.clone());
            (name, path)
        }
        None => (None, None),
    };
    json!({
        "operation_id": op.operation_id.to_string(),
        "namespace_id": namespace_id.to_string(),
        "item_id": op.item_id.map(|id| id.to_string()),
        "item_name": item_name,
        "item_path": item_path,
        "operation_type": format!("{:?}", op.operation_type),
        "state": format!("{:?}", op.state),
        "priority": op.priority,
        "attempt_count": op.attempt_count,
        "last_error_message": op.last_error_message,
        "updated_at": op.updated_at,
    })
}

fn parse_operation_state(raw: &str) -> Option<OperationState> {
    Some(match raw {
        "Pending" => OperationState::Pending,
        "WaitingRetry" => OperationState::WaitingRetry,
        "WaitingNetwork" => OperationState::WaitingNetwork,
        "WaitingAuthentication" => OperationState::WaitingAuthentication,
        "FailedPermanent" => OperationState::FailedPermanent,
        _ => return None,
    })
}

fn parse_operation_type(raw: &str) -> Option<OperationType> {
    Some(match raw {
        "UploadFile" => OperationType::UploadFile,
        "CreateDirectory" => OperationType::CreateDirectory,
        "MoveItem" => OperationType::MoveItem,
        "RenameItem" => OperationType::RenameItem,
        "DeleteItem" => OperationType::DeleteItem,
        "RestoreItem" => OperationType::RestoreItem,
        "HydrateItem" => OperationType::HydrateItem,
        "PinTree" => OperationType::PinTree,
        "RefreshChanges" => OperationType::RefreshChanges,
        "ReconcileNamespace" => OperationType::ReconcileNamespace,
        _ => return None,
    })
}

/// Página pedida por padrão quando `limit` não vem na query — grande o
/// bastante para preencher a tela sem paginar toda hora, pequena o
/// bastante para nunca reintroduzir o travamento de T7-05.
const DEFAULT_OPERATIONS_PAGE_SIZE: u32 = 50;

#[derive(serde::Deserialize)]
struct OperationsQuery {
    limit: Option<u32>,
    offset: Option<u32>,
    /// Mesmos valores de `state` no JSON de resposta (`Pending`,
    /// `WaitingRetry`, ...) — um valor desconhecido é tratado como "sem
    /// filtro" em vez de erro 400, para nunca quebrar a UI por um typo.
    state: Option<String>,
    /// Idem, mesmos valores de `operation_type` na resposta.
    operation_type: Option<String>,
    /// Substring do nome do arquivo/pasta (não do caminho completo).
    search: Option<String>,
}

/// `GET /v1/operations?limit=&offset=&state=&operation_type=&search=`
/// (SPEC §20.3, T7-06): página das operações do journal ainda não
/// concluídas, agregadas de todos os namespaces montados — mais as que já
/// falharam de vez (`FailedPermanent`, sempre primeiro na página, T7-04).
/// Toda operação carrega `last_error_message`, quando existe, para que a
/// causa não dependa de olhar o log — ex.: `HTTP 411 Length Required`
/// visto em produção. Paginado (T7-06): uma conta com milhares de
/// operações pendentes espalhadas por muitas pastas fazia o endpoint
/// nunca responder mesmo já com cache de caminho — resolver nome/caminho
/// (o que é caro) agora só acontece para a página pedida; `total`/
/// `total_failed` vêm de `COUNT(*)` (barato) e continuam batendo mesmo com
/// a página truncada.
async fn get_operations(State(state): State<AppState>, Query(query): Query<OperationsQuery>) -> (StatusCode, Json<Value>) {
    let filter = nexofs_sync_core::OperationsFilter {
        state: query.state.as_deref().and_then(parse_operation_state),
        operation_type: query.operation_type.as_deref().and_then(parse_operation_type),
        search: query.search,
    };
    let limit = query.limit.unwrap_or(DEFAULT_OPERATIONS_PAGE_SIZE);
    let offset = query.offset.unwrap_or(0);

    let mut operations: Vec<Value> = Vec::new();
    let mut total: u64 = 0;
    let mut total_failed: u64 = 0;
    for (namespace_id, sync_core) in state.namespaces_snapshot().await.iter() {
        let page = match sync_core.list_operations_page(filter.clone(), limit, offset).await {
            Ok(page) => page,
            Err(err) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": {"code": "INTERNAL", "message": err.to_string()}}))),
        };
        total += page.total;
        total_failed += page.total_failed;

        // Um único cache de itens por namespace/página — ver
        // `item_relative_path_cached`. Como a página já vem limitada
        // (`DEFAULT_OPERATIONS_PAGE_SIZE`/`limit`), isto nunca mais escala
        // com o tamanho total da fila.
        let mut item_cache = std::collections::HashMap::new();
        for op in page.operations {
            operations.push(operation_to_json(namespace_id, sync_core, op, &mut item_cache).await);
        }
    }
    (StatusCode::OK, Json(json!({ "operations": operations, "total": total, "total_failed": total_failed })))
}

fn parse_operation_id(raw: &str) -> Result<OperationId, (StatusCode, Json<Value>)> {
    uuid::Uuid::parse_str(raw)
        .map(OperationId)
        .map_err(|_| (StatusCode::BAD_REQUEST, Json(json!({"error": {"code": "INVALID_ID", "message": "operation_id inválido"}}))))
}

/// `POST /v1/operations/{id}/retry` (SPEC §20.3): endereçado só pelo
/// `operation_id`, sem `namespace_id` na URL — procura em todos os
/// namespaces montados, mesmo padrão de `post_resolve_conflict`.
async fn post_retry_operation(State(state): State<AppState>, Path(operation_id_raw): Path<String>) -> (StatusCode, Json<Value>) {
    let operation_id = match parse_operation_id(&operation_id_raw) {
        Ok(id) => id,
        Err(response) => return response,
    };
    for sync_core in state.namespaces_snapshot().await.values() {
        match sync_core.retry_operation(operation_id).await {
            Ok(true) => return (StatusCode::OK, Json(json!({"status": "ok"}))),
            Ok(false) => continue,
            Err(err) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": {"code": "INTERNAL", "message": err.to_string()}}))),
        }
    }
    (
        StatusCode::NOT_FOUND,
        Json(json!({"error": {"code": "NOT_FOUND", "message": "operação não encontrada ou não está num estado que aceite retry"}})),
    )
}

/// `POST /v1/operations/{id}/cancel` (SPEC §20.3).
async fn post_cancel_operation(State(state): State<AppState>, Path(operation_id_raw): Path<String>) -> (StatusCode, Json<Value>) {
    let operation_id = match parse_operation_id(&operation_id_raw) {
        Ok(id) => id,
        Err(response) => return response,
    };
    for sync_core in state.namespaces_snapshot().await.values() {
        match sync_core.cancel_operation(operation_id).await {
            Ok(true) => return (StatusCode::OK, Json(json!({"status": "ok"}))),
            Ok(false) => continue,
            Err(err) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": {"code": "INTERNAL", "message": err.to_string()}}))),
        }
    }
    (
        StatusCode::NOT_FOUND,
        Json(json!({"error": {"code": "NOT_FOUND", "message": "operação não encontrada ou já está em voo/concluída"}})),
    )
}

/// `GET /v1/cache` (SPEC §20.3/FR-CACHE-005): uso agregado de cache por
/// namespace montado.
async fn get_cache(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
    let mut cache: Vec<Value> = Vec::new();
    for (namespace_id, sync_core) in state.namespaces_snapshot().await.iter() {
        let stats = match sync_core.cache_stats().await {
            Ok(stats) => stats,
            Err(err) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": {"code": "INTERNAL", "message": err.to_string()}}))),
        };
        // T5-07: detalhamento por camada (limpo/modificado localmente/
        // mantido localmente/parcial) além do total já usado por T2-15.
        let breakdown = match sync_core.cache_breakdown().await {
            Ok(breakdown) => breakdown,
            Err(err) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": {"code": "INTERNAL", "message": err.to_string()}}))),
        };
        cache.push(json!({
            "namespace_id": namespace_id.to_string(),
            "hydrated_items": stats.hydrated_items,
            "hydrated_bytes": stats.hydrated_bytes,
            "clean_items": breakdown.clean_items,
            "clean_bytes": breakdown.clean_bytes,
            "dirty_items": breakdown.dirty_items,
            "dirty_bytes": breakdown.dirty_bytes,
            "partial_items": breakdown.partial_items,
            "partial_bytes": breakdown.partial_bytes,
            "overlay_items": breakdown.overlay_items,
            "overlay_bytes": breakdown.overlay_bytes,
        }));
    }
    (StatusCode::OK, Json(json!({ "cache": cache, "max_bytes_per_namespace": state.cache_max_bytes })))
}

/// `POST /v1/cache/cleanup` (SPEC §20.3): mesmo mecanismo do tick periódico
/// de manutenção (T4-11), disparado manualmente para todos os namespaces.
async fn post_cache_cleanup(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
    for sync_core in state.namespaces_snapshot().await.values() {
        if let Err(err) = sync_core.enforce_cache_quota(state.cache_max_bytes).await {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": {"code": "INTERNAL", "message": err.to_string()}})));
        }
    }
    (StatusCode::OK, Json(json!({"status": "ok"})))
}

/// `GET /v1/events` (T5-02/SPEC §20.4): stream SSE do barramento
/// compartilhado — um assinante que fique para trás demais (`Lagged`)
/// simplesmente perde os eventos mais antigos e volta a receber os
/// seguintes; o stream nunca é a única fonte de verdade, só um atalho de
/// UX sobre o que uma chamada REST normal também mostra.
async fn get_events(State(state): State<AppState>) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let receiver = state.event_bus.subscribe();
    let stream = tokio_stream::wrappers::BroadcastStream::new(receiver).filter_map(|item| match item {
        Ok(event) => serde_json::to_string(&event).ok().map(|json| Ok(Event::default().data(json))),
        Err(_lagged) => None,
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// `PRETTY_NAME` de `/etc/os-release` (padrão freedesktop.org em toda
/// distro-alvo da SPEC — Fedora/Ubuntu/KDE Neon) — melhor esforço: um
/// pacote de diagnóstico sem essa linha ainda é útil, não vale falhar por
/// causa dela.
fn distro_pretty_name() -> Option<String> {
    let content = std::fs::read_to_string("/etc/os-release").ok()?;
    content.lines().find_map(|line| line.strip_prefix("PRETTY_NAME=")).map(|v| v.trim_matches('"').to_string())
}

fn kernel_release() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/osrelease").ok().map(|s| s.trim().to_string())
}

/// Melhor esforço: só existe sob uma sessão de systemd user + journald;
/// nenhum dos dois é garantido (ex.: rodando fora de uma unidade systemd),
/// e a ausência não deve impedir o resto do pacote de ser gerado. SPEC
/// §23.3 "logs recentes redigidos" — a redação em si já acontece na
/// emissão (`SecretToken` nunca imprime o segredo em `Debug`/`Display`),
/// então isto só recorta as últimas linhas, sem filtrar de novo.
async fn recent_daemon_logs() -> Vec<String> {
    let output = tokio::process::Command::new("journalctl")
        .args(["--user", "-u", "nexofsd", "-n", "200", "--no-pager", "--output=short-iso"])
        .output()
        .await;
    match output {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout).lines().map(str::to_string).collect(),
        _ => Vec::new(),
    }
}

/// `POST /v1/diagnostics/package` (T5-10/SPEC §23.3). Nunca inclui conteúdo
/// de arquivo do usuário nem segredo — só metadados/agregados que já
/// atravessam o resto da API local.
async fn post_diagnostics_package(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
    let mut namespaces: Vec<Value> = Vec::new();
    let (namespace_summaries, cores) = {
        let mounted = state.mounted.read().await;
        (mounted.namespace_summaries.clone(), mounted.namespaces.clone())
    };
    for summary in namespace_summaries.iter() {
        let Some(sync_core) = cores.get(&summary.namespace_id) else { continue };
        match sync_core.diagnostics_snapshot().await {
            Ok(snapshot) => namespaces.push(json!({
                "namespace_id": summary.namespace_id.to_string(),
                "display_name": summary.display_name,
                "mount_state": summary.mount_state,
                "diagnostics": snapshot,
            })),
            Err(err) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": {"code": "INTERNAL", "message": err.to_string()}}))),
        }
    }

    let scopes: Vec<Value> = state
        .governor
        .snapshot()
        .into_iter()
        .map(|m| {
            json!({
                "provider_id": m.scope.provider_id.as_ref(),
                "operation_class": format!("{:?}", m.scope.operation_class),
                "in_flight": m.in_flight,
                "circuit_state": format!("{:?}", m.circuit_state),
            })
        })
        .collect();

    let package = json!({
        "version": env!("CARGO_PKG_VERSION"),
        "generated_at_unix": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
        "distro": distro_pretty_name(),
        "kernel": kernel_release(),
        "desktop": std::env::var("XDG_CURRENT_DESKTOP").ok(),
        "session_type": std::env::var("XDG_SESSION_TYPE").ok(),
        "namespaces": namespaces,
        "queues_and_circuit_breakers": scopes,
        "recent_daemon_logs": recent_daemon_logs().await,
    });

    let filename = format!("diagnostico-{}.json", package["generated_at_unix"]);
    let saved_path = state.diagnostics_dir.join(filename);
    if let Err(err) = tokio::fs::create_dir_all(&state.diagnostics_dir).await {
        tracing::warn!(%err, "não foi possível garantir o diretório de diagnósticos — pacote só retornado na resposta");
    } else if let Err(err) = tokio::fs::write(&saved_path, serde_json::to_vec_pretty(&package).unwrap_or_default()).await {
        tracing::warn!(%err, "não foi possível salvar cópia do pacote de diagnóstico em disco");
    }

    (StatusCode::OK, Json(json!({ "package": package, "saved_to": saved_path.to_string_lossy() })))
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/status", get(get_status))
        .route("/v1/metrics", get(get_metrics))
        .route("/v1/accounts", get(get_accounts))
        .route("/v1/accounts/auth/start", post(post_accounts_auth_start))
        .route("/v1/accounts/{id}/unmount", post(post_unmount_account))
        .route("/v1/accounts/{id}/remount", post(post_remount_account))
        .route("/v1/accounts/{id}", axum::routing::delete(delete_account))
        .route("/v1/namespaces", get(get_namespaces))
        .route("/v1/operations", get(get_operations))
        .route("/v1/operations/{id}/retry", post(post_retry_operation))
        .route("/v1/operations/{id}/cancel", post(post_cancel_operation))
        .route("/v1/cache", get(get_cache))
        .route("/v1/cache/cleanup", post(post_cache_cleanup))
        .route("/v1/events", get(get_events))
        .route("/v1/diagnostics/package", post(post_diagnostics_package))
        .route("/v1/namespaces/{id}/refresh", post(post_refresh))
        .route("/v1/namespaces/{id}/sync-now", post(post_sync_now))
        .route("/v1/namespaces/{id}/ignore-rules", get(get_ignore_rules).post(post_add_ignore_rule))
        .route("/v1/namespaces/{id}/ignore-rules/{rule_id}", axum::routing::delete(delete_ignore_rule))
        .route("/v1/namespaces/{id}/ignore-profiles/suggestions", get(get_ignore_profile_suggestions))
        .route("/v1/namespaces/{id}/ignore-profiles/apply", post(post_apply_ignore_profile))
        .route("/v1/namespaces/{id}/storm-paused-folders", get(get_storm_paused_folders))
        .route("/v1/namespaces/{id}/storm-resume", post(post_resume_storm))
        .route("/v1/namespaces/{id}/items", get(get_namespace_items))
        .route("/v1/namespaces/{id}/pin", post(post_set_pin_state))
        .route("/v1/namespaces/{id}/conflicts", get(get_conflicts))
        .route("/v1/conflicts", get(get_all_conflicts))
        .route("/v1/conflicts/{id}/resolve", post(post_resolve_conflict))
        .with_state(state)
}
