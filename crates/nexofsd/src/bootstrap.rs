//! Fluxo de "adicionar conta" mínimo da Fase 1: autentica (ou retoma sessão
//! via refresh token), materializa provider/account/namespace no índice
//! local e determina o ponto de montagem. A UI gráfica (Fase 5) substituirá
//! este fluxo por linha de comando por telas — a lógica abaixo é a mesma.

use nexofs_auth::{generate_state, LoopbackListener, PkceVerifier};
use nexofs_domain::{AccountId, NamespaceId, SecretToken};
use nexofs_metadata_store::MetadataStore;
use nexofs_provider_api::{AuthenticationRequest, AuthenticatedAccount, CloudProvider, RemoteNamespace};
use nexofs_provider_googledrive::GoogleDriveProvider;
use nexofs_provider_onedrive::OneDriveProvider;
use rusqlite::{params, OptionalExtension};
use std::path::PathBuf;
use std::sync::Arc;

// client_id não é segredo (ver docs/adr/0013 e a explicação dada ao
// usuário): é o identificador público do app NexoFS perante a Microsoft,
// embutido no binário como qualquer cliente OAuth público faz (rclone,
// Insync, etc). Pode ser sobreposto via NEXOFS_ONEDRIVE_CLIENT_ID para outra
// instalação do app registration. O tenant usado é `common` por padrão
// (contas pessoais + corporativas de qualquer organização, FR-ACC-001 +
// FR-ACC-002) — exige que o app registration no Azure Portal esteja
// configurado como multi-tenant + contas pessoais.
const DEFAULT_CLIENT_ID: &str = "f3bcaf96-18b5-4baa-b278-60c63e3be52e";

const KEYRING_SERVICE: &str = "nexofs";

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("relógio do sistema não pode estar antes de 1970")
        .as_secs() as i64
}

pub fn build_onedrive_provider() -> Arc<OneDriveProvider> {
    let config = nexofs_provider_onedrive::OneDriveConfig::from_env_or_defaults(DEFAULT_CLIENT_ID);
    Arc::new(OneDriveProvider::new(config))
}

/// T7-02: sem `client_id` embutido — diferente do OneDrive, o Google exige
/// registrar um projeto próprio no Google Cloud Console (não existe um
/// "app público de referência" equivalente ao `rclone` que possamos embutir
/// aqui sem ser a própria conta Google Cloud de quem publica o NexoFS).
/// `NEXOFS_GOOGLEDRIVE_CLIENT_ID` é portanto obrigatória — sem ela,
/// `build_googledrive_provider` ainda constrói o provider (para
/// `descriptor()`/testes funcionarem), mas `build_authorization_url`
/// produziria uma URL com `client_id` vazio, que o Google rejeita cedo,
/// com uma mensagem clara.
pub fn build_googledrive_provider() -> Arc<GoogleDriveProvider> {
    let config = nexofs_provider_googledrive::GoogleDriveConfig::from_env();
    Arc::new(GoogleDriveProvider::new(config))
}

/// T7-02: registro simples de provedores conhecidos — `nexofsd` monta cada
/// conta de acordo com `accounts.provider_id`, escolhendo o adaptador certo
/// aqui em vez de ter o tipo concreto espalhado pelo resto do processo
/// (que só conhece `dyn CloudProvider`, ver `nexofs-sync-core`/T7-03).
pub fn build_provider_registry() -> std::collections::HashMap<String, Arc<dyn CloudProvider>> {
    let mut providers: std::collections::HashMap<String, Arc<dyn CloudProvider>> = std::collections::HashMap::new();
    let onedrive = build_onedrive_provider();
    providers.insert(onedrive.descriptor().id.to_string(), onedrive);
    let googledrive = build_googledrive_provider();
    providers.insert(googledrive.descriptor().id.to_string(), googledrive);
    providers
}

/// Um identificador de conta já persistida — o suficiente para tentar
/// retomar a sessão sem repetir o login interativo (T2-11: múltiplas
/// contas, cada uma com seu próprio ciclo de autenticação independente).
#[derive(Debug, Clone)]
pub struct StoredAccount {
    pub account_id: AccountId,
    pub provider_id: String,
    pub display_name: String,
}

/// T7-02: generalizado de `list_onedrive_accounts` — todas as contas de
/// todos os provedores, não só OneDrive. Quem chama decide o que fazer com
/// `provider_id` (tipicamente: procurar o adaptador certo em
/// `build_provider_registry()`).
pub async fn list_all_accounts(store: &MetadataStore) -> anyhow::Result<Vec<StoredAccount>> {
    let rows: Vec<(String, String, String)> = store
        .read(|conn| {
            let mut stmt = conn.prepare("SELECT account_id, provider_id, display_name FROM accounts")?;
            let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })
        .await?;

    Ok(rows
        .into_iter()
        .map(|(id_s, provider_id, display_name)| StoredAccount {
            account_id: AccountId(uuid::Uuid::parse_str(&id_s).expect("account_id armazenado é sempre um UUID válido")),
            provider_id,
            display_name,
        })
        .collect())
}

/// Tenta retomar a sessão de uma conta específica via keyring. `Ok(None)`
/// quando não há refresh token guardado; erro quando há token mas ele foi
/// recusado (revogado/expirado) — em ambos os casos, quem chama decide se
/// isso deve virar um login interativo ou apenas pular esta conta (para não
/// bloquear as demais contas já montadas, FR-ACC-003/004).
pub async fn try_refresh_session(
    provider: &dyn CloudProvider,
    account_id: &str,
) -> anyhow::Result<Option<AuthenticatedAccount>> {
    let refresh_token = tokio::task::spawn_blocking({
        let account_id = account_id.to_string();
        move || nexofs_auth::load_refresh_token(KEYRING_SERVICE, &account_id)
    })
    .await??;

    let Some(refresh_token) = refresh_token else {
        return Ok(None);
    };

    match provider.refresh_via_refresh_token(&refresh_token).await {
        Ok(authenticated) => {
            tracing::info!(account_id, provider = %provider.descriptor().id, "sessão retomada via refresh token — sem login interativo");
            Ok(Some(authenticated))
        }
        Err(err) => {
            tracing::warn!(account_id, %err, "refresh token inválido ou expirado para esta conta");
            Ok(None)
        }
    }
}

/// Conduz o fluxo completo de Authorization Code + PKCE via navegador do
/// sistema (NFR-SEC-001) — usado tanto para a primeira conta quanto para
/// adicionar contas seguintes (`NEXOFS_ADD_ACCOUNT=1`), de qualquer provedor.
pub async fn interactive_login(provider: &dyn CloudProvider) -> anyhow::Result<AuthenticatedAccount> {
    let listener = LoopbackListener::bind().await?;
    let redirect_uri = listener.redirect_uri();
    let verifier = PkceVerifier::generate();
    let state = generate_state();
    let authorize_url = provider.build_authorization_url(&redirect_uri, &state, &verifier.challenge_s256());

    tracing::info!(provider = %provider.descriptor().id, "abra esta URL no navegador para autenticar:");
    tracing::info!("{authorize_url}");
    if let Err(err) = nexofs_auth::open_system_browser(&authorize_url) {
        tracing::warn!(%err, "não foi possível abrir o navegador automaticamente — use a URL acima manualmente");
    }

    let received = listener.receive_authorization_code(&state).await?;
    let authenticated = provider
        .authenticate(AuthenticationRequest {
            authorization_code: SecretToken::new(received.code),
            pkce_verifier: verifier.verifier().clone(),
            redirect_uri,
        })
        .await?;

    Ok(authenticated)
}

/// `INSERT ... ON CONFLICT DO NOTHING` para cada provedor conhecido —
/// idempotente entre reinícios do daemon.
pub async fn upsert_provider_rows(store: &MetadataStore) -> anyhow::Result<()> {
    for (provider_id, display_name) in [("onedrive", "Microsoft OneDrive"), ("googledrive", "Google Drive")] {
        let now = now_unix();
        let provider_id = provider_id.to_string();
        let display_name = display_name.to_string();
        store
            .write(move |tx| {
                tx.execute(
                    "INSERT INTO providers (provider_id, display_name, capabilities_json, created_at, updated_at) \
                     VALUES (?1, ?2, '{}', ?3, ?3) \
                     ON CONFLICT(provider_id) DO NOTHING",
                    params![provider_id, display_name, now],
                )
            })
            .await?;
    }
    Ok(())
}

/// Reaproveita o `account_id` local existente quando a mesma conta remota já
/// foi vista antes (chave `UNIQUE(provider_id, provider_account_id)`),
/// preservando a identidade dos namespaces/itens já indexados.
pub async fn upsert_account_row(
    store: &MetadataStore,
    provider_id: &str,
    authenticated: &AuthenticatedAccount,
) -> anyhow::Result<AccountId> {
    let provider_id_owned = provider_id.to_string();
    let provider_account_id = authenticated.provider_account_id.clone();
    let existing: Option<String> = store
        .read(move |conn| {
            conn.query_row(
                "SELECT account_id FROM accounts WHERE provider_id = ?1 AND provider_account_id = ?2",
                params![provider_id_owned, provider_account_id],
                |row| row.get(0),
            )
            .optional()
        })
        .await?;

    let account_id = match existing {
        Some(id_s) => AccountId(uuid::Uuid::parse_str(&id_s).expect("account_id armazenado é sempre um UUID válido")),
        None => AccountId::new(),
    };

    let account_id_s = account_id.to_string();
    let provider_id_owned = provider_id.to_string();
    let provider_account_id = authenticated.provider_account_id.clone();
    let display_name = authenticated.display_name.clone();
    let tenant_id = authenticated.tenant_id.clone();
    let now = now_unix();
    store
        .write(move |tx| {
            tx.execute(
                "INSERT INTO accounts (account_id, provider_id, provider_account_id, account_type, display_name, tenant_id, auth_state, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, 'PERSONAL', ?4, ?5, 'VALID', ?6, ?6) \
                 ON CONFLICT(provider_id, provider_account_id) DO UPDATE SET \
                    display_name = excluded.display_name, tenant_id = excluded.tenant_id, \
                    auth_state = 'VALID', updated_at = excluded.updated_at",
                params![account_id_s, provider_id_owned, provider_account_id, display_name, tenant_id, now],
            )
        })
        .await?;

    Ok(account_id)
}

pub async fn store_refresh_token(account_id: AccountId, refresh_token: SecretToken) -> anyhow::Result<()> {
    let account_id_s = account_id.to_string();
    tokio::task::spawn_blocking(move || nexofs_auth::store_refresh_token(KEYRING_SERVICE, &account_id_s, &refresh_token))
        .await??;
    Ok(())
}

/// Namespace já indexado anteriormente, com o ponto de montagem que foi
/// gravado da última vez — nunca recalculado a partir do padrão, para que
/// uma mudança de `mount_path` (manual hoje, por UI/CLI futuramente,
/// SPEC §8.1 "o usuário PODE configurar outro diretório") sobreviva a
/// reinícios do daemon.
pub async fn find_existing_namespace(
    store: &MetadataStore,
    account_id: AccountId,
    remote_namespace_id: &str,
) -> anyhow::Result<Option<(NamespaceId, PathBuf, String)>> {
    let account_id_s = account_id.to_string();
    let remote_namespace_id = remote_namespace_id.to_string();
    let row: Option<(String, String, String)> = store
        .read(move |conn| {
            conn.query_row(
                "SELECT namespace_id, mount_path, display_name FROM namespaces WHERE account_id = ?1 AND remote_namespace_id = ?2",
                params![account_id_s, remote_namespace_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
        })
        .await?;

    Ok(row.map(|(id_s, path_s, display_name)| {
        (
            NamespaceId(uuid::Uuid::parse_str(&id_s).expect("namespace_id armazenado é sempre um UUID válido")),
            PathBuf::from(path_s),
            display_name,
        )
    }))
}

/// Cria a linha de namespace na primeira vez que esta conta/drive é vista.
/// `mount_path` só é gravado aqui — chamadas seguintes devem usar
/// `find_existing_namespace` em vez de recalcular o padrão.
pub async fn insert_namespace_row(
    store: &MetadataStore,
    account_id: AccountId,
    namespace: &RemoteNamespace,
    mount_path: &PathBuf,
) -> anyhow::Result<NamespaceId> {
    let namespace_id = NamespaceId::new();
    let namespace_id_s = namespace_id.to_string();
    let account_id_s = account_id.to_string();
    let remote_namespace_id = namespace.remote_namespace_id.clone();
    let display_name = namespace.display_name.clone();
    let mount_path_s = mount_path.to_string_lossy().to_string();
    let now = now_unix();
    store
        .write(move |tx| {
            tx.execute(
                "INSERT INTO namespaces (namespace_id, account_id, remote_namespace_id, display_name, namespace_type, mount_path, mount_state, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, 'PERSONAL', ?5, 'MOUNTING', ?6, ?6)",
                params![namespace_id_s, account_id_s, remote_namespace_id, display_name, mount_path_s, now],
            )
        })
        .await?;

    Ok(namespace_id)
}

pub async fn mark_namespace_mounted(store: &MetadataStore, namespace_id: NamespaceId) -> anyhow::Result<()> {
    set_namespace_mount_state(store, namespace_id, "MOUNTED").await
}

/// T5-desktop ("desmontar"/"remontar"): grava o estado sem desfazer o
/// registro do namespace — ao contrário de `delete_account_and_data`, a
/// conta continua existindo e pode ser remontada depois sem repetir login
/// interativo.
pub async fn set_namespace_mount_state(store: &MetadataStore, namespace_id: NamespaceId, mount_state: &str) -> anyhow::Result<()> {
    let namespace_id_s = namespace_id.to_string();
    let mount_state = mount_state.to_string();
    store
        .write(move |tx| tx.execute("UPDATE namespaces SET mount_state = ?2 WHERE namespace_id = ?1", params![namespace_id_s, mount_state]))
        .await?;
    Ok(())
}

/// O namespace já indexado para uma conta (hoje sempre exatamente um por
/// conta OneDrive — um drive). Usado na inicialização para decidir, sem
/// nenhuma chamada de rede, se uma conta deve ser montada de novo ou pulada
/// porque o usuário pediu para desmontá-la explicitamente.
pub struct StoredNamespace {
    pub namespace_id: NamespaceId,
    pub display_name: String,
    pub mount_path: PathBuf,
    pub mount_state: String,
}

pub async fn find_namespace_for_account(store: &MetadataStore, account_id: AccountId) -> anyhow::Result<Option<StoredNamespace>> {
    let account_id_s = account_id.to_string();
    let row: Option<(String, String, String, String)> = store
        .read(move |conn| {
            conn.query_row(
                "SELECT namespace_id, display_name, mount_path, mount_state FROM namespaces WHERE account_id = ?1",
                [account_id_s],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()
        })
        .await?;

    Ok(row.map(|(id_s, display_name, mount_path, mount_state)| StoredNamespace {
        namespace_id: NamespaceId(uuid::Uuid::parse_str(&id_s).expect("namespace_id armazenado é sempre um UUID válido")),
        display_name,
        mount_path: PathBuf::from(mount_path),
        mount_state,
    }))
}

/// T5-desktop ("excluir conta"): remove a conta e todo o índice local dela
/// (namespaces/itens/operações/conflitos/regras — nunca os arquivos de
/// verdade no `mount_path`, que ficam no disco como estavam) e apaga o
/// refresh token do keyring (NFR-SEC-007). `foreign_keys = ON` (SPEC §10.5)
/// exige apagar os filhos antes dos pais, dentro de uma única transação —
/// uma falha no meio não deixa a conta "meio excluída".
pub async fn delete_account_and_data(store: &MetadataStore, account_id: AccountId) -> anyhow::Result<()> {
    let account_id_s = account_id.to_string();
    let account_id_for_keyring = account_id_s.clone();
    store
        .write(move |tx| {
            tx.execute(
                "DELETE FROM inode_map WHERE namespace_id IN (SELECT namespace_id FROM namespaces WHERE account_id = ?1)",
                [&account_id_s],
            )?;
            tx.execute(
                "DELETE FROM ignore_rules WHERE namespace_id IN (SELECT namespace_id FROM namespaces WHERE account_id = ?1)",
                [&account_id_s],
            )?;
            tx.execute(
                "DELETE FROM conflicts WHERE namespace_id IN (SELECT namespace_id FROM namespaces WHERE account_id = ?1)",
                [&account_id_s],
            )?;
            tx.execute(
                "DELETE FROM operations WHERE namespace_id IN (SELECT namespace_id FROM namespaces WHERE account_id = ?1)",
                [&account_id_s],
            )?;
            tx.execute(
                "DELETE FROM local_states WHERE item_id IN (SELECT item_id FROM items WHERE namespace_id IN (SELECT namespace_id FROM namespaces WHERE account_id = ?1))",
                [&account_id_s],
            )?;
            tx.execute(
                "DELETE FROM items WHERE namespace_id IN (SELECT namespace_id FROM namespaces WHERE account_id = ?1)",
                [&account_id_s],
            )?;
            tx.execute("DELETE FROM namespaces WHERE account_id = ?1", [&account_id_s])?;
            tx.execute("DELETE FROM accounts WHERE account_id = ?1", [&account_id_s])?;
            Ok(())
        })
        .await?;

    tokio::task::spawn_blocking(move || nexofs_auth::delete_refresh_token(KEYRING_SERVICE, &account_id_for_keyring)).await??;
    Ok(())
}

/// Ponto de montagem para um namespace visto pela primeira vez:
/// `NEXOFS_MOUNT_PATH`, quando definida, ou `$HOME/NexoFS/<nome-sanitizado>`
/// (SPEC §8.1). Sem UI/CLI ainda para escolher o diretório interativamente
/// (isso chega na Fase 5), a variável de ambiente é a forma de configurar
/// disponível hoje — o valor gravado no banco na primeira execução é o que
/// vale dali em diante, mesmo que a variável mude depois.
pub fn default_mount_path_for(display_name: &str) -> anyhow::Result<PathBuf> {
    if let Ok(custom) = std::env::var("NEXOFS_MOUNT_PATH") {
        return Ok(PathBuf::from(custom));
    }

    let home = std::env::var("HOME").map_err(|_| anyhow::anyhow!("HOME não definido"))?;
    let safe_name: String = display_name
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    let safe_name = if safe_name.is_empty() { "Conta".to_string() } else { safe_name };
    Ok(PathBuf::from(home).join("NexoFS").join(safe_name))
}
