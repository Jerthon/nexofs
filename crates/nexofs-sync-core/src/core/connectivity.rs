//! Detecção de conectividade (T3-09, FR-OFF-005). Antes desta fase o
//! "offline" só existia implicitamente — uma operação presa em
//! `WaitingRetry` sem nenhum sinal explícito de por quê. Aqui toda chamada
//! ao provedor que passa pelo Governor é observada num único ponto
//! (`execute_governed`), atualizando um sinal booleano simples; a transição
//! offline→online libera imediatamente qualquer operação `WaitingNetwork`
//! deste namespace para o próximo tick do dispatcher, em vez de esperar o
//! backoff correr até o fim.

use super::SyncCore;
use nexofs_api_governor::{Priority, RateScope};
use nexofs_provider_api::{ProviderErrorKind, ProviderResult};
use std::sync::atomic::Ordering;

impl SyncCore {
    /// Único ponto de entrada do núcleo para chamadas ao provedor através do
    /// Governor — todo `self.governor.execute(...)` deveria passar por aqui,
    /// para que a conectividade seja observada de forma uniforme em vez de
    /// repetir a lógica em cada chamador.
    pub(super) async fn execute_governed<T, F, Fut>(&self, scope: RateScope, priority: Priority, call: F) -> ProviderResult<T>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = ProviderResult<T>>,
    {
        let result = self.governor.execute(scope, priority, call).await;
        self.observe_connectivity(&result).await;
        result
    }

    /// Qualquer resultado que não seja `Network`/`Timeout` prova que a
    /// chamada alcançou o provedor e voltou com uma resposta real — mesmo um
    /// erro de negócio (`NotFound`, `VersionConflict`, `RateLimited`, ...)
    /// já basta para considerar a conectividade presente.
    async fn observe_connectivity<T>(&self, result: &ProviderResult<T>) {
        let currently_online = !matches!(result, Err(err) if matches!(err.kind, ProviderErrorKind::Network | ProviderErrorKind::Timeout));
        let was_online = self.online.swap(currently_online, Ordering::SeqCst);

        if !was_online && currently_online {
            tracing::info!(namespace_id = %self.ctx.namespace_id, "conectividade recuperada — retomando operações em espera de rede");
            if let Err(err) = self.wake_waiting_network_operations().await {
                tracing::warn!(%err, "falha ao acordar operações WAITING_NETWORK após reconexão");
            }
        } else if was_online && !currently_online {
            tracing::warn!(namespace_id = %self.ctx.namespace_id, "sem conectividade — operações de rede entrarão em espera até a reconexão");
        }
    }

    /// FR-OFF-005/`/v1/metrics`: sinal explícito de conectividade deste
    /// namespace — `false` significa que a última chamada ao provedor falhou
    /// por rede/timeout e nenhuma chamada bem-sucedida a substituiu ainda.
    pub async fn is_online(&self) -> bool {
        self.online.load(Ordering::SeqCst)
    }
}
