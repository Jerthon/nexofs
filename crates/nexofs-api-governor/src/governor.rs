//! Núcleo do `ProviderApiGovernor` (SPEC §7.4): admissão por prioridade
//! real (não apenas FIFO), token bucket, circuit breaker por escopo e
//! respeito estrito a `Retry-After`.

use crate::circuit_breaker::{Admission, CircuitBreaker};
use crate::priority_queue::PriorityQueue;
use crate::scope::{OperationClass, Priority, RateScope};
use crate::token_bucket::TokenBucket;
use nexofs_provider_api::{ProviderError, ProviderErrorKind, ProviderResult};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

struct AdmissionState {
    in_use: usize,
    limit: usize,
    waiters: PriorityQueue<oneshot::Sender<()>>,
}

struct ScopeState {
    admission: Mutex<AdmissionState>,
    token_bucket: TokenBucket,
    circuit: CircuitBreaker,
}

impl ScopeState {
    fn new(limit: usize) -> Self {
        Self {
            admission: Mutex::new(AdmissionState {
                in_use: 0,
                limit,
                waiters: PriorityQueue::new(),
            }),
            // Capacidade de rajada = limite de concorrência; taxa sustentada
            // conservadora de metade disso por segundo — evita que o token
            // bucket seja mais permissivo que o próprio limite de
            // concorrência seria sozinho (SPEC §7.8 valores são o teto).
            token_bucket: TokenBucket::new(limit.max(1) as u32, (limit.max(1) as f64) / 2.0),
            circuit: CircuitBreaker::new(),
        }
    }

    async fn acquire_admission(self: &Arc<Self>, priority: Priority) {
        let rx = {
            let mut state = self.admission.lock().expect("lock síncrono");
            if state.in_use < state.limit {
                state.in_use += 1;
                None
            } else {
                let (tx, rx) = oneshot::channel();
                state.waiters.push(priority, tx);
                Some(rx)
            }
        };

        if let Some(rx) = rx {
            // O slot é entregue diretamente a este waiter por `release()` —
            // não há nova disputa ao acordar, então não há starvation entre
            // acordar e tentar de novo.
            let _ = rx.await;
        }
    }

    /// Entrega a vaga liberada ao próximo waiter da fila — mas um waiter
    /// pode ter desistido (timeout/cancelamento no chamador) sem removê-lo
    /// da fila; `send` falha nesse caso e a vaga não pode ser considerada
    /// entregue, senão ela vazaria permanentemente. Tenta o próximo até
    /// entregar com sucesso ou esvaziar a fila.
    fn release_admission(&self) {
        let mut state = self.admission.lock().expect("lock síncrono");
        loop {
            match state.waiters.pop() {
                Some(tx) => {
                    if tx.send(()).is_ok() {
                        return;
                    }
                }
                None => {
                    state.in_use = state.in_use.saturating_sub(1);
                    return;
                }
            }
        }
    }
}

/// Mantido enquanto a chamada ao provedor está em andamento; liberar o
/// permit (drop) devolve a vaga — ao waiter de maior prioridade em espera,
/// se houver algum, ou ao contador do escopo.
pub struct GovernorPermit {
    scope_state: Arc<ScopeState>,
}

impl Drop for GovernorPermit {
    fn drop(&mut self) {
        self.scope_state.release_admission();
    }
}

pub struct ProviderApiGovernor {
    scopes: Mutex<HashMap<RateScope, Arc<ScopeState>>>,
    concurrency_overrides: HashMap<OperationClass, usize>,
}

impl Default for ProviderApiGovernor {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderApiGovernor {
    pub fn new() -> Self {
        Self {
            scopes: Mutex::new(HashMap::new()),
            concurrency_overrides: HashMap::new(),
        }
    }

    pub fn with_concurrency_overrides(overrides: HashMap<OperationClass, usize>) -> Self {
        Self {
            scopes: Mutex::new(HashMap::new()),
            concurrency_overrides: overrides,
        }
    }

    fn limit_for(&self, class: OperationClass) -> usize {
        self.concurrency_overrides
            .get(&class)
            .copied()
            .unwrap_or_else(|| class.default_concurrency_limit())
    }

    fn scope_state_for(&self, scope: &RateScope) -> Arc<ScopeState> {
        let mut scopes = self.scopes.lock().expect("lock síncrono");
        scopes
            .entry(scope.clone())
            .or_insert_with(|| Arc::new(ScopeState::new(self.limit_for(scope.operation_class))))
            .clone()
    }

    /// Aquisição de baixo nível — apenas admissão por prioridade e token
    /// bucket, sem checar o circuit breaker nem invocar nada. Útil quando o
    /// chamador quer controlar o ciclo de vida do permit manualmente.
    pub async fn acquire(&self, scope: RateScope) -> GovernorPermit {
        self.acquire_with_priority(scope, OperationClass::InteractiveMetadata.default_priority()).await
    }

    /// Apenas admissão por prioridade + limite de concorrência — sem token
    /// bucket nem circuit breaker. O token bucket fica só em `execute()`,
    /// e só é aplicado *depois* da admissão: aplicá-lo antes deixaria a
    /// suavização de taxa (que não conhece prioridade) decidir a ordem de
    /// despacho no lugar da fila de prioridade.
    pub async fn acquire_with_priority(&self, scope: RateScope, priority: Priority) -> GovernorPermit {
        let scope_state = self.scope_state_for(&scope);
        scope_state.acquire_admission(priority).await;
        GovernorPermit { scope_state }
    }

    /// Caminho recomendado (FR-API-001 a 004): checa o circuit breaker,
    /// aplica token bucket + admissão por prioridade, executa a chamada e
    /// atualiza o circuito com o resultado. Chamadas com erro permanente
    /// (`NotFound`, `InvalidName`, etc.) fecham o circuito — elas provam que
    /// o provedor está respondendo, só que esta operação específica falhou.
    pub async fn execute<T, F, Fut>(&self, scope: RateScope, priority: Priority, call: F) -> ProviderResult<T>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = ProviderResult<T>>,
    {
        let scope_state = self.scope_state_for(&scope);

        if let Admission::Blocked { retry_after } = scope_state.circuit.check() {
            return Err(ProviderError::new(
                ProviderErrorKind::TemporarilyUnavailable {
                    retry_after: Some(retry_after),
                },
                "circuito aberto para este escopo — aguardando recuperação do provedor",
            ));
        }

        scope_state.acquire_admission(priority).await;
        let _permit = GovernorPermit {
            scope_state: scope_state.clone(),
        };
        // Suaviza a taxa *depois* de já ter sido admitido na ordem de
        // prioridade correta — o token bucket não sabe o que é prioridade,
        // então nunca deve decidir quem entra primeiro.
        scope_state.token_bucket.acquire().await;

        // Rede de segurança: sem isto, uma conexão que trava depois de
        // estabelecida (sem RST/FIN) vaza a vaga de concorrência do escopo
        // para sempre — o `_permit` só seria liberado quando `call()`
        // retornasse, o que nunca aconteceria (ver doc de
        // `OperationClass::default_timeout`).
        let result = match tokio::time::timeout(scope.operation_class.default_timeout(), call()).await {
            Ok(result) => result,
            Err(_elapsed) => Err(ProviderError::new(
                ProviderErrorKind::Timeout,
                format!(
                    "chamada excedeu {}s sem responder",
                    scope.operation_class.default_timeout().as_secs()
                ),
            )),
        };

        match &result {
            Ok(_) => scope_state.circuit.record_success(),
            Err(err) => match &err.kind {
                ProviderErrorKind::RateLimited { retry_after } => {
                    scope_state.circuit.record_rate_limited(*retry_after);
                }
                ProviderErrorKind::TemporarilyUnavailable { retry_after } => {
                    scope_state.circuit.record_transient_failure(*retry_after);
                }
                ProviderErrorKind::Timeout => {
                    scope_state.circuit.record_transient_failure(None);
                }
                _ => scope_state.circuit.record_success(),
            },
        }

        result
    }

    /// Quantas vagas de concorrência estão em uso no escopo (métricas/diagnóstico, FR-API-006).
    pub fn in_flight(&self, scope: &RateScope) -> usize {
        let scopes = self.scopes.lock().expect("lock síncrono");
        match scopes.get(scope) {
            Some(state) => state.admission.lock().expect("lock síncrono").in_use,
            None => 0,
        }
    }

    pub fn circuit_state(&self, scope: &RateScope) -> Option<crate::circuit_breaker::CircuitState> {
        let scopes = self.scopes.lock().expect("lock síncrono");
        scopes.get(scope).map(|state| state.circuit.state())
    }

    /// FR-API-006: métricas de todos os escopos já vistos pelo Governor —
    /// só existem entradas para escopos que já tiveram ao menos uma
    /// chamada, então um escopo ausente aqui equivale a "0 em voo,
    /// circuito fechado".
    pub fn snapshot(&self) -> Vec<ScopeMetrics> {
        let scopes = self.scopes.lock().expect("lock síncrono");
        scopes
            .iter()
            .map(|(scope, state)| ScopeMetrics {
                scope: scope.clone(),
                in_flight: state.admission.lock().expect("lock síncrono").in_use,
                circuit_state: state.circuit.state(),
            })
            .collect()
    }
}

pub struct ScopeMetrics {
    pub scope: RateScope,
    pub in_flight: usize,
    pub circuit_state: crate::circuit_breaker::CircuitState,
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexofs_domain::{AccountId, NamespaceId, ProviderId};
    use nexofs_provider_api::ProviderErrorKind;
    use std::time::Duration;

    fn scope(class: OperationClass) -> RateScope {
        RateScope {
            provider_id: ProviderId::from("fake"),
            account_id: AccountId::new(),
            organization_scope: None,
            namespace_id: Some(NamespaceId::new()),
            operation_class: class,
        }
    }

    #[tokio::test]
    async fn respects_concurrency_limit_per_scope() {
        let governor = ProviderApiGovernor::with_concurrency_overrides(
            [(OperationClass::ChangeTracking, 1)].into_iter().collect(),
        );
        let scope = scope(OperationClass::ChangeTracking);

        let permit1 = governor.acquire(scope.clone()).await;
        assert_eq!(governor.in_flight(&scope), 1);

        let second = tokio::time::timeout(Duration::from_millis(50), governor.acquire(scope.clone())).await;
        assert!(second.is_err(), "segunda aquisição não deveria completar com limite 1");

        drop(permit1);
        let permit2 = tokio::time::timeout(Duration::from_millis(50), governor.acquire(scope.clone()))
            .await
            .expect("deve completar após liberar o primeiro permit");
        drop(permit2);
    }

    #[tokio::test]
    async fn different_scopes_never_block_each_other() {
        let governor = ProviderApiGovernor::new();
        let interactive = scope(OperationClass::InteractiveDownload);
        let background = scope(OperationClass::BackgroundIndex);

        let _interactive_permit = governor.acquire(interactive).await;
        let background_permit =
            tokio::time::timeout(Duration::from_millis(50), governor.acquire(background)).await;
        assert!(background_permit.is_ok());
    }

    #[tokio::test]
    async fn higher_priority_waiter_is_admitted_before_lower_priority_one() {
        let governor = Arc::new(ProviderApiGovernor::with_concurrency_overrides(
            [(OperationClass::InteractiveDownload, 1)].into_iter().collect(),
        ));
        let scope = scope(OperationClass::InteractiveDownload);

        // Ocupa a única vaga do escopo.
        let held = governor.acquire(scope.clone()).await;

        let order = Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));

        // Enfileira primeiro o de baixa prioridade (background), depois o
        // de alta prioridade (download interativo) — a ordem de chegada é
        // inversa da ordem de prioridade esperada.
        let bg_governor = governor.clone();
        let bg_scope = scope.clone();
        let bg_order = order.clone();
        let bg_task = tokio::spawn(async move {
            let _permit = bg_governor
                .acquire_with_priority(bg_scope, OperationClass::BackgroundIndex.default_priority())
                .await;
            bg_order.lock().unwrap().push("background");
        });

        tokio::time::sleep(Duration::from_millis(20)).await;

        let hi_governor = governor.clone();
        let hi_scope = scope.clone();
        let hi_order = order.clone();
        let hi_task = tokio::spawn(async move {
            let _permit = hi_governor
                .acquire_with_priority(hi_scope, OperationClass::InteractiveDownload.default_priority())
                .await;
            hi_order.lock().unwrap().push("interactive");
        });

        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(held); // libera a vaga — deve ir para o waiter de maior prioridade (interactive)

        bg_task.await.unwrap();
        hi_task.await.unwrap();

        assert_eq!(*order.lock().unwrap(), vec!["interactive", "background"]);
    }

    #[tokio::test]
    async fn execute_opens_circuit_on_rate_limited_and_blocks_next_call() {
        let governor = ProviderApiGovernor::new();
        let scope = scope(OperationClass::InteractiveMetadata);
        let priority = OperationClass::InteractiveMetadata.default_priority();

        let result: ProviderResult<()> = governor
            .execute(scope.clone(), priority, || async {
                Err(ProviderError::new(
                    ProviderErrorKind::RateLimited {
                        retry_after: Some(Duration::from_secs(30)),
                    },
                    "throttled",
                ))
            })
            .await;
        assert!(result.is_err());

        let second: ProviderResult<()> = governor
            .execute(scope, priority, || async { Ok(()) })
            .await;
        match second {
            Err(e) => assert_eq!(e.kind, ProviderErrorKind::TemporarilyUnavailable { retry_after: second_retry_after(&e) }),
            Ok(_) => panic!("circuito deveria estar aberto e bloquear a segunda chamada"),
        }
    }

    fn second_retry_after(e: &ProviderError) -> Option<Duration> {
        match &e.kind {
            ProviderErrorKind::TemporarilyUnavailable { retry_after } => *retry_after,
            _ => None,
        }
    }

    /// Regressão: uma chamada que trava para sempre (conexão descartada
    /// silenciosamente por um middlebox, sem RST/FIN) não pode vazar a vaga
    /// de concorrência do escopo permanentemente — bug real encontrado
    /// validando a Fase 2 contra o OneDrive (ver `OperationClass::default_timeout`).
    #[tokio::test(start_paused = true)]
    async fn execute_times_out_a_call_that_never_completes_and_releases_the_permit() {
        let governor = ProviderApiGovernor::with_concurrency_overrides(
            [(OperationClass::InteractiveMetadata, 1)].into_iter().collect(),
        );
        let scope = scope(OperationClass::InteractiveMetadata);
        let priority = OperationClass::InteractiveMetadata.default_priority();

        let result: ProviderResult<()> = governor
            .execute(scope.clone(), priority, || std::future::pending())
            .await;

        assert!(matches!(
            result.unwrap_err().kind,
            ProviderErrorKind::Timeout
        ));
        // A vaga foi liberada — uma nova chamada no mesmo escopo não fica
        // presa esperando a que "travou" (ela nunca terminaria).
        let second: ProviderResult<()> = governor.execute(scope, priority, || async { Ok(()) }).await;
        assert!(second.is_ok());
    }

    #[tokio::test]
    async fn execute_closes_circuit_on_permanent_error_like_not_found() {
        let governor = ProviderApiGovernor::new();
        let scope = scope(OperationClass::InteractiveMetadata);
        let priority = OperationClass::InteractiveMetadata.default_priority();

        let _: ProviderResult<()> = governor
            .execute(scope.clone(), priority, || async {
                Err(ProviderError::new(ProviderErrorKind::NotFound, "não existe"))
            })
            .await;

        // NotFound não deve abrir o circuito — a próxima chamada passa normalmente.
        let ok: ProviderResult<()> = governor.execute(scope, priority, || async { Ok(()) }).await;
        assert!(ok.is_ok());
    }
}
