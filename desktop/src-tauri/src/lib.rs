//! T5-03/SPEC §2.2.2 — backend Tauri fino: só fala com a API local do
//! daemon (`nexofs-api-client`, o mesmo cliente que `nexofs-cli` usa).
//! NUNCA acessa SQLite nem guarda refresh token — a UI é só um cliente a
//! mais da mesma API que a CLI e qualquer outra ferramenta usam.

use nexofs_api_client::ApiClient;
use nexofs_domain::paths::NexoFsPaths;
use serde_json::Value;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{TrayIconBuilder, TrayIconEvent};
use tauri::{Emitter, Manager};

struct AppState {
    client: ApiClient,
}

fn api_client() -> Result<ApiClient, String> {
    let paths = NexoFsPaths::from_env();
    let socket_path = paths
        .control_socket_path()
        .ok_or_else(|| "XDG_RUNTIME_DIR não definido nesta sessão — a API local do nexofsd não tem onde escutar".to_string())?;
    Ok(ApiClient::new(socket_path))
}

/// Converte o `anyhow::Error` do cliente numa mensagem simples — o
/// frontend só precisa exibir o texto, não inspecionar a cadeia de erro.
fn to_command_error(err: anyhow::Error) -> String {
    err.to_string()
}

#[tauri::command]
async fn get_status(state: tauri::State<'_, AppState>) -> Result<Value, String> {
    state.client.get("/v1/status").await.map_err(to_command_error)
}

#[tauri::command]
async fn get_accounts(state: tauri::State<'_, AppState>) -> Result<Value, String> {
    state.client.get("/v1/accounts").await.map_err(to_command_error)
}

#[tauri::command]
async fn get_namespaces(state: tauri::State<'_, AppState>) -> Result<Value, String> {
    state.client.get("/v1/namespaces").await.map_err(to_command_error)
}

/// Fica esperando até 5 minutos (o mesmo prazo do lado do daemon) enquanto
/// o usuário conclui o login no navegador — um comando Tauri assíncrono não
/// bloqueia a janela enquanto isso, só esta chamada específica do frontend.
#[tauri::command]
async fn add_account(state: tauri::State<'_, AppState>, provider_id: String, mount_path: Option<String>, display_name: Option<String>) -> Result<Value, String> {
    let body = Some(serde_json::json!({ "provider_id": provider_id, "mount_path": mount_path, "display_name": display_name }));
    state.client.post("/v1/accounts/auth/start", body).await.map_err(to_command_error)
}

#[tauri::command]
async fn unmount_account(state: tauri::State<'_, AppState>, account_id: String) -> Result<Value, String> {
    state.client.post(&format!("/v1/accounts/{account_id}/unmount"), None).await.map_err(to_command_error)
}

#[tauri::command]
async fn remount_account(state: tauri::State<'_, AppState>, account_id: String) -> Result<Value, String> {
    state.client.post(&format!("/v1/accounts/{account_id}/remount"), None).await.map_err(to_command_error)
}

#[tauri::command]
async fn delete_account(state: tauri::State<'_, AppState>, account_id: String) -> Result<Value, String> {
    state.client.delete(&format!("/v1/accounts/{account_id}")).await.map_err(to_command_error)
}

#[tauri::command]
async fn list_items(state: tauri::State<'_, AppState>, namespace_id: String, parent_item_id: Option<String>) -> Result<Value, String> {
    let path = match parent_item_id {
        Some(id) => format!("/v1/namespaces/{namespace_id}/items?parent_item_id={id}"),
        None => format!("/v1/namespaces/{namespace_id}/items"),
    };
    state.client.get(&path).await.map_err(to_command_error)
}

#[tauri::command]
async fn set_pin_state(state: tauri::State<'_, AppState>, namespace_id: String, item_id: String, pin_state: String, recursive: bool) -> Result<Value, String> {
    let body = serde_json::json!({ "item_id": item_id, "pin_state": pin_state, "recursive": recursive });
    state.client.post(&format!("/v1/namespaces/{namespace_id}/pin"), Some(body)).await.map_err(to_command_error)
}

#[tauri::command]
async fn get_operations(state: tauri::State<'_, AppState>) -> Result<Value, String> {
    state.client.get("/v1/operations").await.map_err(to_command_error)
}

#[tauri::command]
async fn retry_operation(state: tauri::State<'_, AppState>, operation_id: String) -> Result<Value, String> {
    state.client.post(&format!("/v1/operations/{operation_id}/retry"), None).await.map_err(to_command_error)
}

#[tauri::command]
async fn cancel_operation(state: tauri::State<'_, AppState>, operation_id: String) -> Result<Value, String> {
    state.client.post(&format!("/v1/operations/{operation_id}/cancel"), None).await.map_err(to_command_error)
}

#[tauri::command]
async fn get_conflicts(state: tauri::State<'_, AppState>) -> Result<Value, String> {
    state.client.get("/v1/conflicts").await.map_err(to_command_error)
}

#[tauri::command]
async fn resolve_conflict(state: tauri::State<'_, AppState>, conflict_id: String, resolution: String) -> Result<Value, String> {
    let body = serde_json::json!({ "resolution": resolution });
    state.client.post(&format!("/v1/conflicts/{conflict_id}/resolve"), Some(body)).await.map_err(to_command_error)
}

#[tauri::command]
async fn get_cache(state: tauri::State<'_, AppState>) -> Result<Value, String> {
    state.client.get("/v1/cache").await.map_err(to_command_error)
}

#[tauri::command]
async fn cleanup_cache(state: tauri::State<'_, AppState>) -> Result<Value, String> {
    state.client.post("/v1/cache/cleanup", None).await.map_err(to_command_error)
}

#[tauri::command]
async fn get_ignore_rules(state: tauri::State<'_, AppState>, namespace_id: String) -> Result<Value, String> {
    state.client.get(&format!("/v1/namespaces/{namespace_id}/ignore-rules")).await.map_err(to_command_error)
}

#[tauri::command]
async fn add_ignore_rule(state: tauri::State<'_, AppState>, namespace_id: String, pattern: String) -> Result<Value, String> {
    let body = serde_json::json!({ "pattern": pattern });
    state.client.post(&format!("/v1/namespaces/{namespace_id}/ignore-rules"), Some(body)).await.map_err(to_command_error)
}

#[tauri::command]
async fn remove_ignore_rule(state: tauri::State<'_, AppState>, namespace_id: String, rule_id: String) -> Result<Value, String> {
    state.client.delete(&format!("/v1/namespaces/{namespace_id}/ignore-rules/{rule_id}")).await.map_err(to_command_error)
}

#[tauri::command]
async fn ignore_profile_suggestions(state: tauri::State<'_, AppState>, namespace_id: String) -> Result<Value, String> {
    state.client.get(&format!("/v1/namespaces/{namespace_id}/ignore-profiles/suggestions")).await.map_err(to_command_error)
}

#[tauri::command]
async fn apply_ignore_profile(state: tauri::State<'_, AppState>, namespace_id: String, manifest_file: String) -> Result<Value, String> {
    let body = serde_json::json!({ "manifest_file": manifest_file });
    state.client.post(&format!("/v1/namespaces/{namespace_id}/ignore-profiles/apply"), Some(body)).await.map_err(to_command_error)
}

#[tauri::command]
async fn generate_diagnostics_package(state: tauri::State<'_, AppState>) -> Result<Value, String> {
    state.client.post("/v1/diagnostics/package", None).await.map_err(to_command_error)
}

#[tauri::command]
async fn refresh_namespace(state: tauri::State<'_, AppState>, namespace_id: String) -> Result<Value, String> {
    state.client.post(&format!("/v1/namespaces/{namespace_id}/refresh"), None).await.map_err(to_command_error)
}

#[tauri::command]
async fn sync_now(state: tauri::State<'_, AppState>, namespace_id: String) -> Result<Value, String> {
    state.client.post(&format!("/v1/namespaces/{namespace_id}/sync-now"), None).await.map_err(to_command_error)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState { client: api_client().expect("XDG_RUNTIME_DIR deve estar definido numa sessão desktop normal") })
        .setup(|app| {
            // T5-08 (FR-UI-003): ícone de bandeja via StatusNotifierItem no
            // Linux (KDE/GNOME com extensão AppIndicator) — a mesma API do
            // Tauri cobre os três SOs, mas aqui só nos importa o
            // comportamento Linux (SPEC/PRD não cobrem Windows/macOS).
            // Clique esquerdo alterna mostrar/ocultar a janela principal;
            // "Sair" é a única forma de encerrar de vez (fechar a janela só
            // esconde — ver o handler de `WindowEvent::CloseRequested`
            // abaixo), pois o NexoFS precisa continuar rodando e mostrando
            // notificações mesmo com a janela fechada, igual a qualquer
            // cliente de sincronização de nuvem convencional.
            let show_item = MenuItem::with_id(app, "show", "Mostrar NexoFS", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "Sair", true, None::<&str>)?;
            let tray_menu = Menu::with_items(app, &[&show_item, &quit_item])?;

            TrayIconBuilder::new()
                .icon(app.default_window_icon().cloned().expect("ícone padrão do app deve existir (bundle.icon em tauri.conf.json)"))
                .menu(&tray_menu)
                .show_menu_on_left_click(false)
                .tooltip("NexoFS")
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => {
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click { button: tauri::tray::MouseButton::Left, button_state: tauri::tray::MouseButtonState::Up, .. } = event {
                        let app = tray.app_handle();
                        if let Some(window) = app.get_webview_window("main") {
                            match window.is_visible() {
                                Ok(true) => {
                                    let _ = window.hide();
                                }
                                _ => {
                                    let _ = window.show();
                                    let _ = window.set_focus();
                                }
                            }
                        }
                    }
                })
                .build(app)?;

            // Fechar a janela (X da barra de título) minimiza para a
            // bandeja em vez de encerrar o processo — sem isso, o ícone de
            // bandeja perderia o sentido (o daemon continua sincronizando
            // sozinho de qualquer forma, mas o usuário não teria como
            // reabrir a UI sem reiniciar o app inteiro).
            if let Some(window) = app.get_webview_window("main") {
                let window_for_handler = window.clone();
                window.on_window_event(move |event| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                        api.prevent_close();
                        let _ = window_for_handler.hide();
                    }
                });
            }

            // T5-02: reencaminha o stream SSE do daemon como eventos Tauri
            // (`nexofs://event`) — a mesma fonte que `nexofs events` na CLI
            // imprime linha a linha, aqui entregue ao frontend via
            // `listen()` em vez de polling.
            let app_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    let Ok(client) = api_client() else {
                        tracing::warn!("não foi possível montar o cliente da API local para o stream de eventos");
                        return;
                    };
                    let on_connected_handle = app_handle.clone();
                    let on_line_handle = app_handle.clone();
                    let result = client
                        .stream_events(
                            move || {
                                // Cobre tanto "app abriu antes do daemon" quanto uma
                                // reconexão após o daemon cair — nenhum dos dois casos
                                // tem um `SyncEvent` próprio para as telas reagirem.
                                let _ = on_connected_handle.emit("nexofs://connected", ());
                            },
                            move |line| {
                                if let Ok(event) = serde_json::from_str::<Value>(line) {
                                    let _ = on_line_handle.emit("nexofs://event", event);
                                }
                            },
                        )
                        .await;
                    if let Err(err) = result {
                        tracing::warn!(%err, "stream de eventos do daemon caiu — tentando de novo em 5s");
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_status,
            get_accounts,
            add_account,
            unmount_account,
            remount_account,
            delete_account,
            get_namespaces,
            list_items,
            set_pin_state,
            get_operations,
            retry_operation,
            cancel_operation,
            get_conflicts,
            resolve_conflict,
            get_cache,
            cleanup_cache,
            get_ignore_rules,
            add_ignore_rule,
            remove_ignore_rule,
            ignore_profile_suggestions,
            apply_ignore_profile,
            generate_diagnostics_package,
            refresh_namespace,
            sync_now,
        ])
        .run(tauri::generate_context!())
        .expect("erro ao rodar a aplicação NexoFS");
}
