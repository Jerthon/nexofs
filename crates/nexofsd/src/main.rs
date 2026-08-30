mod bootstrap;
mod power;
mod telemetry;

use nexofs_api_governor::ProviderApiGovernor;
use nexofs_content_cache::ContentCache;
use nexofs_overlay::LocalOnlyOverlay;
use nexofs_domain::paths::NexoFsPaths;
use nexofs_domain::{AccountId, NamespaceId};
use nexofs_metadata_store::MetadataStore;
use nexofs_provider_api::{CloudProvider, ProviderAccountContext};
use nexofs_sync_core::{SyncCore, SyncCoreContext};
use std::collections::HashMap;
use std::sync::Arc;

/// T7-02: um adaptador por `provider_id` conhecido (`"onedrive"`,
/// `"googledrive"`) — `nexofsd` escolhe o adaptador certo por conta a partir
/// de `accounts.provider_id`, o resto do processo só conhece `dyn
/// CloudProvider` (SPEC §5.1/T7-03: nenhum tipo concreto de provedor vaza
/// para fora deste módulo).
type ProviderRegistry = HashMap<String, Arc<dyn CloudProvider>>;

/// Mantém vivos os recursos de uma conta montada — dropar `session`
/// desmonta o FUSE; abortar `maintenance` para a manutenção periódica.
struct MountedAccount {
    provider_id: String,
    display_name: String,
    /// Nome de exibição do namespace/montagem — normalmente igual a
    /// `display_name` (o nome da conta), mas pode ter sido customizado na
    /// tela de "adicionar conta" (T5-desktop, "nome da montagem").
    namespace_display_name: String,
    namespace_id: NamespaceId,
    account_id: AccountId,
    mount_path: std::path::PathBuf,
    sync_core: Arc<SyncCore>,
    session: nexofs_fuse::BackgroundSession,
    maintenance: tokio::task::JoinHandle<()>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    telemetry::init();

    let paths = NexoFsPaths::from_env();
    paths.ensure_data_dirs()?;
    paths.ensure_runtime_dir()?;

    let sqlite_path = paths.sqlite_path();
    tracing::info!(path = %sqlite_path.display(), "abrindo metadata store");
    let store = Arc::new(MetadataStore::open(sqlite_path)?);

    bootstrap::upsert_provider_rows(&store).await?;

    let providers: Arc<ProviderRegistry> = Arc::new(bootstrap::build_provider_registry());

    // Primeira execução (nenhuma conta ainda) ou pedido explícito de
    // adicionar mais uma conta (T2-11: múltiplas contas independentes) —
    // ambos passam pelo mesmo login interativo via navegador.
    // NEXOFS_ADD_ACCOUNT_PROVIDER (T7-02): qual adaptador usar neste fluxo
    // baseado em variável de ambiente — "onedrive" por padrão, para não
    // mudar o comportamento de quem já usava isso antes do Google Drive
    // existir.
    let existing_accounts = bootstrap::list_all_accounts(&store).await?;
    let should_add_account = existing_accounts.is_empty() || std::env::var("NEXOFS_ADD_ACCOUNT").is_ok();
    if should_add_account {
        let provider_id = std::env::var("NEXOFS_ADD_ACCOUNT_PROVIDER").unwrap_or_else(|_| "onedrive".to_string());
        let provider = providers
            .get(&provider_id)
            .ok_or_else(|| anyhow::anyhow!("provedor desconhecido em NEXOFS_ADD_ACCOUNT_PROVIDER: {provider_id} (use 'onedrive' ou 'googledrive')"))?;
        let authenticated = bootstrap::interactive_login(provider.as_ref()).await?;
        tracing::info!(conta = %authenticated.display_name, provider = %provider_id, "conta autenticada");
        let account_id = bootstrap::upsert_account_row(&store, &provider_id, &authenticated).await?;
        bootstrap::store_refresh_token(account_id, authenticated.refresh_token.clone()).await?;
    }

    let accounts = bootstrap::list_all_accounts(&store).await?;
    if accounts.is_empty() {
        anyhow::bail!("nenhuma conta configurada — defina NEXOFS_ADD_ACCOUNT=1 para adicionar a primeira");
    }

    // Governor e cache são compartilhados entre contas: RateScope já inclui
    // account_id (isolamento correto sem duplicar estruturas), e
    // cache_object_id é um ItemId (UUID), único globalmente mesmo entre
    // contas diferentes.
    let governor = Arc::new(ProviderApiGovernor::new());
    let cache_max_bytes: u64 = std::env::var("NEXOFS_CACHE_MAX_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2 * 1024 * 1024 * 1024);
    // T5-02: um único barramento para todos os namespaces montados neste
    // processo — `GET /v1/events` assina este, não um por conta.
    let event_bus = Arc::new(nexofs_sync_core::EventBus::new());

    let mut mounted = Vec::new();
    // T5-desktop ("desmontar"): uma conta cujo namespace ficou marcado
    // `UNMOUNTED` na última execução (pedido explícito do usuário, não uma
    // falha) não é remontada sozinha na inicialização — ela só volta a
    // aparecer, sem FUSE nenhum, para que a UI ofereça "remontar".
    let mut unmounted_accounts = Vec::new();
    for account in accounts {
        let Some(provider) = providers.get(&account.provider_id) else {
            tracing::error!(conta = %account.display_name, provider = %account.provider_id, "provedor desconhecido para esta conta — pulando");
            continue;
        };
        match bootstrap::find_namespace_for_account(&store, account.account_id).await? {
            Some(ns) if ns.mount_state == "UNMOUNTED" => {
                tracing::info!(conta = %account.display_name, "conta desmontada explicitamente — pulando montagem na inicialização");
                unmounted_accounts.push((account, ns));
                continue;
            }
            _ => {}
        }
        match mount_account(&store, provider, &paths, governor.clone(), event_bus.clone(), cache_max_bytes, account.account_id, &account.display_name).await {
            Ok(handle) => mounted.push(handle),
            Err(err) => tracing::error!(conta = %account.display_name, %err, "falha ao montar esta conta — as demais continuam"),
        }
    }

    if mounted.is_empty() && unmounted_accounts.is_empty() {
        anyhow::bail!("nenhuma conta pôde ser montada");
    }

    for account in &mounted {
        tracing::info!(conta = %account.display_name, mountpoint = %account.session.mountpoint.display(), "nexofsd pronto — navegue no ponto de montagem");
    }

    let namespaces: HashMap<NamespaceId, Arc<SyncCore>> =
        mounted.iter().map(|a| (a.namespace_id, a.sync_core.clone())).collect();
    // T5-01/SPEC §20.3: `GET /v1/accounts`/`GET /v1/namespaces` — hoje uma
    // conta OneDrive tem sempre exatamente um namespace (o drive), então a
    // lista de contas é só a projeção dos campos comuns dos namespaces.
    let mut namespace_summaries: Vec<nexofs_local_api::NamespaceSummary> = mounted
        .iter()
        .map(|a| nexofs_local_api::NamespaceSummary {
            namespace_id: a.namespace_id,
            account_id: a.account_id,
            display_name: a.namespace_display_name.clone(),
            mount_path: a.mount_path.to_string_lossy().to_string(),
            mount_state: "MOUNTED".to_string(),
        })
        .collect();
    let mut account_summaries: Vec<nexofs_local_api::AccountSummary> = mounted
        .iter()
        .map(|a| nexofs_local_api::AccountSummary { account_id: a.account_id, provider_id: a.provider_id.clone(), display_name: a.display_name.clone() })
        .collect();
    for (account, ns) in &unmounted_accounts {
        namespace_summaries.push(nexofs_local_api::NamespaceSummary {
            namespace_id: ns.namespace_id,
            account_id: account.account_id,
            display_name: ns.display_name.clone(),
            mount_path: ns.mount_path.to_string_lossy().to_string(),
            mount_state: "UNMOUNTED".to_string(),
        });
        account_summaries.push(nexofs_local_api::AccountSummary {
            account_id: account.account_id,
            provider_id: account.provider_id.clone(),
            display_name: account.display_name.clone(),
        });
    }

    // T5-desktop ("adicionar conta"): `mounted` precisa sobreviver a partir
    // de agora tanto para o desligamento limpo (unmount de todas, inclusive
    // as adicionadas depois) quanto para a task de adicionar conta abaixo
    // guardar a sessão FUSE nova — um `Arc<Mutex<_>>` em vez do `Vec` local
    // de antes.
    let mounted = Arc::new(tokio::sync::Mutex::new(mounted));
    let (add_account_tx, add_account_rx) = tokio::sync::mpsc::channel(4);
    let (account_control_tx, account_control_rx) = tokio::sync::mpsc::channel(4);

    let local_api_task = match paths.control_socket_path() {
        Some(socket_path) => {
            paths.ensure_runtime_dir()?;
            let state = nexofs_local_api::AppState::new(
                namespaces,
                account_summaries,
                namespace_summaries,
                governor.clone(),
                event_bus.clone(),
                cache_max_bytes,
                paths.diagnostics_dir(),
            )
            .with_add_account_channel(add_account_tx)
            .with_account_control_channel(account_control_tx);

            tokio::spawn(handle_add_account_requests(
                add_account_rx,
                store.clone(),
                providers.clone(),
                paths.clone(),
                governor.clone(),
                event_bus.clone(),
                cache_max_bytes,
                mounted.clone(),
                state.clone(),
            ));

            tokio::spawn(handle_account_control_requests(
                account_control_rx,
                store.clone(),
                providers.clone(),
                paths.clone(),
                governor.clone(),
                event_bus.clone(),
                cache_max_bytes,
                mounted.clone(),
                state.clone(),
            ));

            Some(tokio::spawn(async move {
                if let Err(err) = nexofs_local_api::serve(&socket_path, state).await {
                    tracing::error!(%err, "API local encerrou com erro");
                }
            }))
        }
        None => {
            tracing::warn!("XDG_RUNTIME_DIR não definido — API local indisponível nesta sessão");
            None
        }
    };

    // T5-11/SPEC §2.2.4: `Type=notify` — sem isto, o systemd considera a
    // unidade "iniciada" assim que o processo existe, não quando ela está
    // de fato pronta (montagens FUSE + API local no ar); silenciosamente
    // ignorado quando não roda sob systemd (`$NOTIFY_SOCKET` ausente, ex.:
    // `cargo run` direto no terminal).
    if let Err(err) = sd_notify::notify(false, &[sd_notify::NotifyState::Ready]) {
        tracing::debug!(%err, "sd_notify READY não enviado — provavelmente não estamos rodando sob systemd");
    }

    wait_for_shutdown_signal().await;
    tracing::info!("encerrando nexofsd — desmontando todas as contas");
    let _ = sd_notify::notify(false, &[sd_notify::NotifyState::Stopping]);
    if let Some(task) = local_api_task {
        task.abort();
    }
    for account in mounted.lock().await.drain(..) {
        account.maintenance.abort();
        drop(account.session);
    }
    Ok(())
}

/// Consome pedidos de `POST /v1/accounts/auth/start` um de cada vez —
/// serializado de propósito, para nunca ter dois logins interativos
/// disputando o mesmo listener de loopback OAuth ao mesmo tempo. Cada
/// pedido é o fluxo inteiro (`bootstrap::interactive_login` até o FUSE
/// montado); erro em qualquer etapa vira a mensagem devolvida ao HTTP, sem
/// afetar as contas já montadas.
#[allow(clippy::too_many_arguments)]
async fn handle_add_account_requests(
    mut requests: tokio::sync::mpsc::Receiver<nexofs_local_api::AddAccountRequest>,
    store: Arc<MetadataStore>,
    providers: Arc<ProviderRegistry>,
    paths: NexoFsPaths,
    governor: Arc<ProviderApiGovernor>,
    event_bus: Arc<nexofs_sync_core::EventBus>,
    cache_max_bytes: u64,
    mounted: Arc<tokio::sync::Mutex<Vec<MountedAccount>>>,
    api_state: nexofs_local_api::AppState,
) {
    while let Some(request) = requests.recv().await {
        let result = match providers.get(&request.provider_id) {
            Some(provider) => {
                add_one_account(
                    &store,
                    provider,
                    &paths,
                    governor.clone(),
                    event_bus.clone(),
                    cache_max_bytes,
                    &mounted,
                    &api_state,
                    request.mount_path,
                    request.display_name,
                )
                .await
            }
            None => Err(anyhow::anyhow!("provedor desconhecido: {} (use 'onedrive' ou 'googledrive')", request.provider_id)),
        };
        let response = result.map_err(|err| err.to_string());
        let _ = request.respond_to.send(response);
    }
}

#[allow(clippy::too_many_arguments)]
async fn add_one_account(
    store: &Arc<MetadataStore>,
    provider: &Arc<dyn CloudProvider>,
    paths: &NexoFsPaths,
    governor: Arc<ProviderApiGovernor>,
    event_bus: Arc<nexofs_sync_core::EventBus>,
    cache_max_bytes: u64,
    mounted: &Arc<tokio::sync::Mutex<Vec<MountedAccount>>>,
    api_state: &nexofs_local_api::AppState,
    mount_path_override: Option<std::path::PathBuf>,
    display_name_override: Option<String>,
) -> anyhow::Result<nexofs_local_api::NamespaceSummary> {
    let provider_id = provider.descriptor().id.to_string();
    let authenticated = bootstrap::interactive_login(provider.as_ref()).await?;
    tracing::info!(conta = %authenticated.display_name, provider = %provider_id, "conta autenticada (adicionada em runtime)");
    let account_id = bootstrap::upsert_account_row(store, &provider_id, &authenticated).await?;
    bootstrap::store_refresh_token(account_id, authenticated.refresh_token.clone()).await?;

    let account_ctx = ProviderAccountContext {
        account_id,
        provider_account_id: authenticated.provider_account_id.clone(),
        tenant_id: authenticated.tenant_id.clone(),
        access_token: authenticated.access_token.clone(),
    };

    let handle = mount_account_with_context(
        store,
        provider,
        paths,
        governor,
        event_bus.clone(),
        cache_max_bytes,
        account_id,
        &authenticated.display_name,
        account_ctx,
        authenticated.refresh_token.clone(),
        mount_path_override,
        display_name_override,
    )
    .await?;

    let namespace_summary = nexofs_local_api::NamespaceSummary {
        namespace_id: handle.namespace_id,
        account_id: handle.account_id,
        display_name: handle.namespace_display_name.clone(),
        mount_path: handle.mount_path.to_string_lossy().to_string(),
        mount_state: "MOUNTED".to_string(),
    };
    let account_summary =
        nexofs_local_api::AccountSummary { account_id: handle.account_id, provider_id: handle.provider_id.clone(), display_name: handle.display_name.clone() };

    api_state.insert_mounted(handle.namespace_id, handle.sync_core.clone(), account_summary, namespace_summary.clone()).await;
    event_bus.publish(nexofs_sync_core::SyncEvent::NamespaceMounted { namespace_id: handle.namespace_id });
    mounted.lock().await.push(handle);

    Ok(namespace_summary)
}

/// Consome `AccountControlRequest` um de cada vez (mesmo motivo de
/// `handle_add_account_requests`: nunca duas ações mexendo em `mounted` ao
/// mesmo tempo sem coordenação).
#[allow(clippy::too_many_arguments)]
async fn handle_account_control_requests(
    mut requests: tokio::sync::mpsc::Receiver<nexofs_local_api::AccountControlRequest>,
    store: Arc<MetadataStore>,
    providers: Arc<ProviderRegistry>,
    paths: NexoFsPaths,
    governor: Arc<ProviderApiGovernor>,
    event_bus: Arc<nexofs_sync_core::EventBus>,
    cache_max_bytes: u64,
    mounted: Arc<tokio::sync::Mutex<Vec<MountedAccount>>>,
    api_state: nexofs_local_api::AppState,
) {
    use nexofs_local_api::AccountControlRequest;

    while let Some(request) = requests.recv().await {
        match request {
            AccountControlRequest::Unmount { account_id, respond_to } => {
                let result = unmount_one_account(&store, &mounted, &api_state, account_id).await;
                let _ = respond_to.send(result.map_err(|err| err.to_string()));
            }
            AccountControlRequest::Remount { account_id, respond_to } => {
                let result =
                    remount_one_account(&store, &providers, &paths, governor.clone(), event_bus.clone(), cache_max_bytes, &mounted, &api_state, account_id).await;
                let _ = respond_to.send(result.map_err(|err| err.to_string()));
            }
            AccountControlRequest::Delete { account_id, respond_to } => {
                let result = delete_one_account(&store, &mounted, &api_state, account_id).await;
                let _ = respond_to.send(result.map_err(|err| err.to_string()));
            }
        }
    }
}

/// Desmonta o FUSE desta conta (se estiver montada) e grava `UNMOUNTED` —
/// a conta e o refresh token continuam intactos, só não há mais um
/// `MountedAccount` vivo para ela.
async fn unmount_one_account(
    store: &Arc<MetadataStore>,
    mounted: &Arc<tokio::sync::Mutex<Vec<MountedAccount>>>,
    api_state: &nexofs_local_api::AppState,
    account_id: AccountId,
) -> anyhow::Result<()> {
    let mut guard = mounted.lock().await;
    let position = guard.iter().position(|a| a.account_id == account_id).ok_or_else(|| anyhow::anyhow!("conta não está montada"))?;
    let account = guard.remove(position);
    drop(guard);

    account.maintenance.abort();
    drop(account.session);
    bootstrap::set_namespace_mount_state(store, account.namespace_id, "UNMOUNTED").await?;
    api_state.mark_unmounted(account.namespace_id).await;
    tracing::info!(conta = %account.display_name, "conta desmontada a pedido do usuário");
    Ok(())
}

/// Retoma a sessão via refresh token — o mesmo caminho de `mount_account`
/// usado na inicialização, só que disparado por `POST /v1/accounts/{id}/remount`
/// em vez de acontecer sozinho no boot do daemon.
#[allow(clippy::too_many_arguments)]
async fn remount_one_account(
    store: &Arc<MetadataStore>,
    providers: &Arc<ProviderRegistry>,
    paths: &NexoFsPaths,
    governor: Arc<ProviderApiGovernor>,
    event_bus: Arc<nexofs_sync_core::EventBus>,
    cache_max_bytes: u64,
    mounted: &Arc<tokio::sync::Mutex<Vec<MountedAccount>>>,
    api_state: &nexofs_local_api::AppState,
    account_id: AccountId,
) -> anyhow::Result<nexofs_local_api::NamespaceSummary> {
    if mounted.lock().await.iter().any(|a| a.account_id == account_id) {
        anyhow::bail!("conta já está montada");
    }
    let accounts = bootstrap::list_all_accounts(store).await?;
    let account = accounts.into_iter().find(|a| a.account_id == account_id).ok_or_else(|| anyhow::anyhow!("conta desconhecida"))?;
    let provider = providers
        .get(&account.provider_id)
        .ok_or_else(|| anyhow::anyhow!("provedor desconhecido para esta conta: {}", account.provider_id))?;

    let handle = mount_account(store, provider, paths, governor, event_bus.clone(), cache_max_bytes, account_id, &account.display_name).await?;

    let namespace_summary = nexofs_local_api::NamespaceSummary {
        namespace_id: handle.namespace_id,
        account_id: handle.account_id,
        display_name: handle.namespace_display_name.clone(),
        mount_path: handle.mount_path.to_string_lossy().to_string(),
        mount_state: "MOUNTED".to_string(),
    };
    let account_summary =
        nexofs_local_api::AccountSummary { account_id: handle.account_id, provider_id: handle.provider_id.clone(), display_name: handle.display_name.clone() };

    api_state.insert_mounted(handle.namespace_id, handle.sync_core.clone(), account_summary, namespace_summary.clone()).await;
    event_bus.publish(nexofs_sync_core::SyncEvent::NamespaceMounted { namespace_id: handle.namespace_id });
    mounted.lock().await.push(handle);

    Ok(namespace_summary)
}

/// Desmonta (se preciso) e apaga a conta por completo — índice local e
/// refresh token, nunca os arquivos de verdade em `mount_path`.
async fn delete_one_account(
    store: &Arc<MetadataStore>,
    mounted: &Arc<tokio::sync::Mutex<Vec<MountedAccount>>>,
    api_state: &nexofs_local_api::AppState,
    account_id: AccountId,
) -> anyhow::Result<()> {
    let mut guard = mounted.lock().await;
    if let Some(position) = guard.iter().position(|a| a.account_id == account_id) {
        let account = guard.remove(position);
        drop(guard);
        account.maintenance.abort();
        drop(account.session);
    } else {
        drop(guard);
    }

    bootstrap::delete_account_and_data(store, account_id).await?;
    api_state.remove_account(account_id).await;
    tracing::info!(%account_id, "conta excluída a pedido do usuário");
    Ok(())
}

/// Autentica (via refresh token; nunca login interativo aqui — isso já
/// aconteceu antes, se necessário), resolve o namespace e monta o FUSE para
/// uma única conta. Retorna erro sem efeito colateral nas demais contas.
async fn mount_account(
    store: &Arc<MetadataStore>,
    provider: &Arc<dyn CloudProvider>,
    paths: &NexoFsPaths,
    governor: Arc<ProviderApiGovernor>,
    event_bus: Arc<nexofs_sync_core::EventBus>,
    cache_max_bytes: u64,
    account_id: AccountId,
    display_name: &str,
) -> anyhow::Result<MountedAccount> {
    let authenticated = bootstrap::try_refresh_session(provider.as_ref(), &account_id.to_string())
        .await?
        .ok_or_else(|| anyhow::anyhow!("refresh token ausente ou inválido — reautentique com NEXOFS_ADD_ACCOUNT=1"))?;

    let account_ctx = ProviderAccountContext {
        account_id,
        provider_account_id: authenticated.provider_account_id.clone(),
        tenant_id: authenticated.tenant_id.clone(),
        access_token: authenticated.access_token.clone(),
    };

    mount_account_with_context(
        store,
        provider,
        paths,
        governor,
        event_bus,
        cache_max_bytes,
        account_id,
        display_name,
        account_ctx,
        authenticated.refresh_token.clone(),
        None,
        None,
    )
    .await
}

/// Metade de `mount_account` que não depende de já ter um refresh token
/// salvo — reaproveitada por `POST /v1/accounts/auth/start`, cujo
/// `account_ctx` vem direto do login interativo que acabou de acontecer,
/// não de uma sessão retomada. `mount_path_override`/`namespace_name_override`
/// só têm efeito na primeira vez que este namespace é visto (T5-desktop,
/// "escolher local e nome da montagem") — uma conta já indexada continua
/// usando o que foi gravado da primeira vez, igual a `find_existing_namespace`
/// sempre fez.
#[allow(clippy::too_many_arguments)]
async fn mount_account_with_context(
    store: &Arc<MetadataStore>,
    provider: &Arc<dyn CloudProvider>,
    paths: &NexoFsPaths,
    governor: Arc<ProviderApiGovernor>,
    event_bus: Arc<nexofs_sync_core::EventBus>,
    cache_max_bytes: u64,
    account_id: AccountId,
    display_name: &str,
    account_ctx: ProviderAccountContext,
    refresh_token: nexofs_provider_api::SecretToken,
    mount_path_override: Option<std::path::PathBuf>,
    namespace_name_override: Option<String>,
) -> anyhow::Result<MountedAccount> {
    let namespaces = provider.list_namespaces(&account_ctx).await?;
    let mut namespace = namespaces.into_iter().next().ok_or_else(|| anyhow::anyhow!("a conta não possui nenhum namespace acessível"))?;

    let existing = bootstrap::find_existing_namespace(store, account_id, &namespace.remote_namespace_id).await?;
    let (namespace_id, mount_path, namespace_display_name) = match existing {
        Some((namespace_id, mount_path, stored_display_name)) => (namespace_id, mount_path, stored_display_name),
        None => {
            if let Some(name) = namespace_name_override {
                namespace.display_name = name;
            }
            let mount_path = match mount_path_override {
                Some(path) => path,
                None => bootstrap::default_mount_path_for(display_name)?,
            };
            let namespace_id = bootstrap::insert_namespace_row(store, account_id, &namespace, &mount_path).await?;
            (namespace_id, mount_path, namespace.display_name.clone())
        }
    };
    tokio::fs::create_dir_all(&mount_path).await?;

    let cache = ContentCache::new(paths.cache_clean_dir(), paths.cache_partial_dir(), paths.cache_dirty_dir());
    let overlay = LocalOnlyOverlay::new(paths.overlay_dir(&namespace_id.to_string()));
    let provider_id = provider.descriptor().id;

    let sync_core = Arc::new(
        SyncCore::new(
            store.clone(),
            provider.clone(),
            governor,
            cache,
            overlay,
            account_ctx,
            SyncCoreContext {
                provider_id: provider_id.clone(),
                account_id,
                namespace_id,
                namespace_remote_id: namespace.remote_namespace_id.clone(),
            },
        )
        .with_event_bus(event_bus)
        .with_refresh_token(refresh_token),
    );

    let root_item_id = sync_core.bootstrap_root().await?;

    // SPEC §13.5 pt.1: operações que ficaram `Running` numa queda anterior
    // (ex.: `kill -9` em pleno upload) voltam para `Pending` para serem
    // retomadas pelo dispatcher — nunca ficam presas indefinidamente.
    let recovered = sync_core.recover_running_operations().await?;
    if recovered > 0 {
        tracing::warn!(conta = display_name, recovered, "operações Running recuperadas para Pending após reinício");
    }

    // Bug real de produção: exclusões bloqueadas por conflito sem o
    // registro correspondente em `conflicts` ficavam escondidas para
    // sempre, sem a aba "Conflitos" ter como mostrá-las.
    let backfilled_conflicts = sync_core.backfill_missing_delete_conflicts().await?;
    if backfilled_conflicts > 0 {
        tracing::warn!(conta = display_name, backfilled_conflicts, "conflitos de exclusão reconciliados — estavam bloqueados sem registro");
    }

    tracing::info!(conta = display_name, mountpoint = %mount_path.display(), "montando filesystem FUSE");
    let session = nexofs_fuse::mount(sync_core.clone(), root_item_id, &mount_path, provider_id.clone(), account_id, namespace_id)?;

    bootstrap::mark_namespace_mounted(store, namespace_id).await?;
    let maintenance = tokio::spawn(run_background_maintenance(sync_core.clone(), cache_max_bytes));

    Ok(MountedAccount {
        provider_id: provider_id.to_string(),
        display_name: display_name.to_string(),
        namespace_display_name,
        namespace_id,
        account_id,
        mount_path,
        sync_core,
        session,
        maintenance,
    })
}

/// Política "periódica" de FR-ACT-005 — a mais simples das quatro previstas
/// (browser-aware, qualquer acesso, periódico, manual). Refresh incremental
/// a cada 60s (TTL inicial de FR-ACT-006) e checagem de quota de cache a
/// cada 5 min; ambos idempotentes e seguros de repetir.
async fn run_background_maintenance(sync_core: Arc<SyncCore>, cache_max_bytes: u64) {
    const REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);
    const QUOTA_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(300);
    // T3-06: intervalo curto — é o journal de escrita local (create/write/
    // rename/delete) que fica pendente até este tick rodar; o usuário sente
    // isso como "quanto tempo até aparecer no OneDrive".
    const DISPATCH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
    // T4-14: espaço em disco pode mudar por causa de QUALQUER programa na
    // máquina, não só do NexoFS — checagem mais frequente que a quota
    // própria (que é só política interna), mas sem exagerar (`statvfs` é
    // uma chamada de sistema local, barata, mas ainda assim não de graça).
    const DISK_PRESSURE_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
    // T6-06 ("pausa de baixa prioridade em bateria"): a verificação
    // incremental periódica é justamente o tipo de trabalho não-interativo
    // que vale a pena tornar menos frequente na bateria — nunca parar de
    // sincronizar de vez (o usuário ainda espera que edições apareçam), só
    // consultar com menos frequência para economizar rádio/energia. `write`
    // do usuário (o `dispatch_ticker`) nunca é afetado por isto — não é
    // "trabalho de fundo", é conteúdo que o próprio usuário pediu para
    // sincronizar.
    const BATTERY_REFRESH_MULTIPLIER: u32 = 3;
    // T6-07 ("retomada gradual após suspensão"): um `tick()` que demora
    // muitas vezes mais que o intervalo configurado é o sinal prático de
    // que o processo ficou suspenso (não há um evento de "acordou" sem
    // integrar com logind/D-Bus) — o valor real desta detecção é só
    // diagnóstico (log claro do que aconteceu); a garantia de "sem rajada
    // descontrolada" já vem de graça da arquitetura existente: o
    // `refresh_changes` disparado aqui é uma chamada só, e
    // `dispatch_pending_operations` sempre despacha uma operação por vez
    // através do Governor (T3-09), nunca em paralelo, suspensão ou não.
    const SUSPECTED_SUSPEND_THRESHOLD: std::time::Duration = std::time::Duration::from_secs(REFRESH_INTERVAL.as_secs() * 3);

    let mut refresh_ticker = tokio::time::interval(REFRESH_INTERVAL);
    let mut quota_ticker = tokio::time::interval(QUOTA_CHECK_INTERVAL);
    let mut dispatch_ticker = tokio::time::interval(DISPATCH_INTERVAL);
    let mut disk_pressure_ticker = tokio::time::interval(DISK_PRESSURE_CHECK_INTERVAL);

    let mut refresh_tick_count: u32 = 0;
    let mut last_refresh_tick_at = tokio::time::Instant::now();

    loop {
        tokio::select! {
            _ = refresh_ticker.tick() => {
                let now = tokio::time::Instant::now();
                let gap = now.duration_since(last_refresh_tick_at);
                last_refresh_tick_at = now;
                if gap > SUSPECTED_SUSPEND_THRESHOLD {
                    tracing::info!(namespace_id = %sync_core.namespace_id(), gap = ?gap, "lacuna grande entre ticks de manutenção — provável suspensão do sistema; sincronizando imediatamente");
                }

                refresh_tick_count = refresh_tick_count.wrapping_add(1);
                if power::on_battery() && refresh_tick_count % BATTERY_REFRESH_MULTIPLIER != 0 {
                    continue;
                }
                if let Err(err) = sync_core.refresh_changes().await {
                    tracing::warn!(%err, "falha ao verificar mudanças incrementais");
                }
            }
            _ = quota_ticker.tick() => {
                if let Err(err) = sync_core.enforce_cache_quota(cache_max_bytes).await {
                    tracing::warn!(%err, "falha ao aplicar quota de cache");
                }
            }
            _ = dispatch_ticker.tick() => {
                if let Err(err) = sync_core.dispatch_pending_operations().await {
                    tracing::warn!(%err, "falha ao despachar operações pendentes do journal");
                }
            }
            _ = disk_pressure_ticker.tick() => {
                if let Err(err) = sync_core.handle_disk_pressure(cache_max_bytes).await {
                    tracing::warn!(%err, "falha ao verificar pressão de disco");
                }
            }
        }
    }
}

/// `systemd --user stop` envia `SIGTERM` (SPEC §2.2.4, unidade sem
/// `KillSignal` customizado); um terminal interativo envia `SIGINT` via
/// Ctrl+C. Tratar só `SIGINT` deixaria a montagem FUSE órfã sempre que o
/// systemd parasse o serviço normalmente.
async fn wait_for_shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};

    let mut sigterm = signal(SignalKind::terminate()).expect("registrar handler de SIGTERM");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = sigterm.recv() => {}
    }
}
