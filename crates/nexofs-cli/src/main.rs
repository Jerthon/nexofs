//! T5-09/SPEC §20.3 (`nexofs-cli`) — administração do daemon sem UI
//! gráfica, sobre a mesma API local que `nexofs-desktop` usaria.
//!
//! `mount`/`unmount` de um namespace já existente ainda não têm endpoint
//! no daemon (ver `NexoFS_TASKS_v1.0.md`, T5-01) — os comandos aqui cobrem
//! tudo que a API local já expõe: status, contas/namespaces (incluindo
//! adicionar uma conta nova), operações, conflitos, cache, fixação
//! seletiva, eventos e diagnóstico.

use anyhow::Result;
use clap::{Parser, Subcommand};
use nexofs_api_client::ApiClient;
use nexofs_domain::paths::NexoFsPaths;

#[derive(Parser)]
#[command(name = "nexofs", version, about = "Administração do daemon NexoFS")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Estado geral: contas, filas, cache e circuit breakers.
    Status,
    /// Contas montadas.
    Accounts,
    /// Abre o navegador para autenticar e montar uma conta nova (fica
    /// esperando até você concluir o login, até 5 minutos).
    AccountsAdd {
        /// "onedrive" ou "googledrive".
        #[arg(long, default_value = "onedrive")]
        provider: String,
        /// Onde montar o namespace novo (padrão: $HOME/NexoFS/<nome>).
        #[arg(long)]
        mount_path: Option<String>,
        /// Nome de exibição da montagem/namespace.
        #[arg(long)]
        display_name: Option<String>,
    },
    /// Desmonta uma conta sem excluí-la — dá para remontar depois.
    AccountsUnmount { account_id: String },
    /// Remonta uma conta desmontada, via refresh token salvo (sem navegador).
    AccountsRemount { account_id: String },
    /// Exclui uma conta por completo: desmonta, apaga o índice local e o
    /// refresh token. Nunca apaga os arquivos já sincronizados em disco.
    AccountsDelete { account_id: String },
    /// Namespaces montados.
    Namespaces,
    /// Lista os itens filhos de uma pasta (ou da raiz, se omitida).
    Items { namespace_id: String, parent_item_id: Option<String> },
    /// Fixa um item para ficar sempre disponível localmente (offline).
    Pin {
        namespace_id: String,
        item_id: String,
        /// Também fixa toda a subárvore, se for uma pasta.
        #[arg(long)]
        recursive: bool,
    },
    /// Volta um item para ONLINE_ONLY (deixa de ser mantido localmente).
    Unpin { namespace_id: String, item_id: String },
    /// Atualização incremental manual de um namespace.
    Refresh { namespace_id: String },
    /// Estabiliza e despacha imediatamente as escritas locais pendentes de um namespace.
    SyncNow { namespace_id: String },
    /// Operações do journal ainda não concluídas.
    Operations,
    /// Força retry imediato de uma operação em espera.
    OperationRetry { operation_id: String },
    /// Cancela uma operação ainda não em voo.
    OperationCancel { operation_id: String },
    /// Conflitos abertos.
    Conflicts,
    /// Resolve um conflito aberto.
    ConflictResolve {
        conflict_id: String,
        /// KEEP_LOCAL, KEEP_REMOTE, KEEP_BOTH, SAVE_LOCAL_ELSEWHERE ou DISMISS_TEMPORARILY.
        resolution: String,
    },
    /// Regras de exclusão hoje ativas num namespace.
    IgnoreRules { namespace_id: String },
    /// Adiciona uma regra de exclusão (camada `ACCOUNT`).
    IgnoreRuleAdd { namespace_id: String, pattern: String },
    /// Remove uma regra de exclusão pelo id.
    IgnoreRuleRemove { namespace_id: String, rule_id: String },
    /// Perfis de tecnologia sugeridos a partir de manifestos na raiz.
    IgnoreProfileSuggestions { namespace_id: String },
    /// Aplica um perfil sugerido (confirmação explícita).
    IgnoreProfileApply { namespace_id: String, manifest_file: String },
    /// Uso de cache por namespace.
    Cache,
    /// Força a aplicação da quota de cache agora.
    CacheCleanup,
    /// Gera um pacote de diagnóstico e imprime onde foi salvo.
    Diagnostics,
    /// Acompanha o stream de eventos em tempo real (Ctrl+C para sair).
    Events,
}

fn client() -> Result<ApiClient> {
    let paths = NexoFsPaths::from_env();
    let socket_path = paths
        .control_socket_path()
        .ok_or_else(|| anyhow::anyhow!("XDG_RUNTIME_DIR não definido nesta sessão — a API local do nexofsd não tem onde escutar"))?;
    Ok(ApiClient::new(socket_path))
}

fn print_json(value: &serde_json::Value) {
    println!("{}", serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()));
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let client = client()?;

    match cli.command {
        Command::Status => print_json(&client.get("/v1/status").await?),
        Command::Accounts => print_json(&client.get("/v1/accounts").await?),
        Command::AccountsAdd { provider, mount_path, display_name } => {
            eprintln!("Abrindo o navegador para autenticar — conclua o login para montar a conta.");
            let body = Some(serde_json::json!({ "provider_id": provider, "mount_path": mount_path, "display_name": display_name }));
            print_json(&client.post("/v1/accounts/auth/start", body).await?)
        }
        Command::AccountsUnmount { account_id } => print_json(&client.post(&format!("/v1/accounts/{account_id}/unmount"), None).await?),
        Command::AccountsRemount { account_id } => print_json(&client.post(&format!("/v1/accounts/{account_id}/remount"), None).await?),
        Command::AccountsDelete { account_id } => print_json(&client.delete(&format!("/v1/accounts/{account_id}")).await?),
        Command::Namespaces => print_json(&client.get("/v1/namespaces").await?),
        Command::Items { namespace_id, parent_item_id } => {
            let path = match parent_item_id {
                Some(id) => format!("/v1/namespaces/{namespace_id}/items?parent_item_id={id}"),
                None => format!("/v1/namespaces/{namespace_id}/items"),
            };
            print_json(&client.get(&path).await?)
        }
        Command::Pin { namespace_id, item_id, recursive } => {
            let body = serde_json::json!({ "item_id": item_id, "pin_state": "PINNED", "recursive": recursive });
            print_json(&client.post(&format!("/v1/namespaces/{namespace_id}/pin"), Some(body)).await?)
        }
        Command::Unpin { namespace_id, item_id } => {
            let body = serde_json::json!({ "item_id": item_id, "pin_state": "ONLINE_ONLY" });
            print_json(&client.post(&format!("/v1/namespaces/{namespace_id}/pin"), Some(body)).await?)
        }
        Command::Refresh { namespace_id } => print_json(&client.post(&format!("/v1/namespaces/{namespace_id}/refresh"), None).await?),
        Command::SyncNow { namespace_id } => print_json(&client.post(&format!("/v1/namespaces/{namespace_id}/sync-now"), None).await?),
        Command::Operations => print_json(&client.get("/v1/operations").await?),
        Command::OperationRetry { operation_id } => print_json(&client.post(&format!("/v1/operations/{operation_id}/retry"), None).await?),
        Command::OperationCancel { operation_id } => print_json(&client.post(&format!("/v1/operations/{operation_id}/cancel"), None).await?),
        Command::Conflicts => print_json(&client.get("/v1/conflicts").await?),
        Command::ConflictResolve { conflict_id, resolution } => {
            let body = serde_json::json!({ "resolution": resolution });
            print_json(&client.post(&format!("/v1/conflicts/{conflict_id}/resolve"), Some(body)).await?)
        }
        Command::IgnoreRules { namespace_id } => print_json(&client.get(&format!("/v1/namespaces/{namespace_id}/ignore-rules")).await?),
        Command::IgnoreRuleAdd { namespace_id, pattern } => {
            let body = serde_json::json!({ "pattern": pattern });
            print_json(&client.post(&format!("/v1/namespaces/{namespace_id}/ignore-rules"), Some(body)).await?)
        }
        Command::IgnoreRuleRemove { namespace_id, rule_id } => {
            print_json(&client.delete(&format!("/v1/namespaces/{namespace_id}/ignore-rules/{rule_id}")).await?)
        }
        Command::IgnoreProfileSuggestions { namespace_id } => {
            print_json(&client.get(&format!("/v1/namespaces/{namespace_id}/ignore-profiles/suggestions")).await?)
        }
        Command::IgnoreProfileApply { namespace_id, manifest_file } => {
            let body = serde_json::json!({ "manifest_file": manifest_file });
            print_json(&client.post(&format!("/v1/namespaces/{namespace_id}/ignore-profiles/apply"), Some(body)).await?)
        }
        Command::Cache => print_json(&client.get("/v1/cache").await?),
        Command::CacheCleanup => print_json(&client.post("/v1/cache/cleanup", None).await?),
        Command::Diagnostics => {
            let response = client.post("/v1/diagnostics/package", None).await?;
            match response.get("saved_to").and_then(|v| v.as_str()) {
                Some(path) => println!("Pacote de diagnóstico salvo em: {path}"),
                None => print_json(&response),
            }
        }
        Command::Events => {
            eprintln!("Acompanhando eventos em tempo real — Ctrl+C para sair.");
            client.stream_events(|| {}, |data| println!("{data}")).await?;
        }
    }

    Ok(())
}
