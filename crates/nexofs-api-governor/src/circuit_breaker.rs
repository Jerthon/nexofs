//! Circuit breaker por escopo. SPEC §7.7.
//!
//! Mede a saúde do provedor no escopo, não o sucesso de cada operação
//! individual: um `404`/`InvalidName` fecha o circuito normalmente (o
//! provedor respondeu), enquanto `429`/`503`/timeout o abrem.

use rand::Rng;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    Closed,
    Open,
    HalfOpen,
}

/// Quantas falhas transitórias consecutivas (503/timeout, sem `Retry-After`
/// explícito) antes de abrir o circuito — evita abrir no primeiro soluço
/// (SPEC §7.7 regra 3: "após limiar configurado").
const TRANSIENT_FAILURE_THRESHOLD: u32 = 3;
/// Sondas simultâneas permitidas em `HalfOpen` (SPEC §7.7 regra 5: "liberar
/// apenas uma quantidade limitada de probes").
const HALF_OPEN_MAX_PROBES: u32 = 1;
const HALF_OPEN_RETRY_INTERVAL: Duration = Duration::from_millis(250);
const MAX_BACKOFF: Duration = Duration::from_secs(64);

pub enum Admission {
    Allowed,
    Blocked { retry_after: Duration },
}

pub struct CircuitBreaker {
    inner: std::sync::Mutex<Inner>,
}

struct Inner {
    state: CircuitState,
    open_until: Option<Instant>,
    consecutive_transient_failures: u32,
    half_open_probes_in_flight: u32,
    backoff_attempt: u32,
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self::new()
    }
}

impl CircuitBreaker {
    pub fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(Inner {
                state: CircuitState::Closed,
                open_until: None,
                consecutive_transient_failures: 0,
                half_open_probes_in_flight: 0,
                backoff_attempt: 0,
            }),
        }
    }

    pub fn state(&self) -> CircuitState {
        self.inner.lock().expect("lock síncrono").state
    }

    /// Deve ser chamado antes de cada tentativa de chamada remota.
    pub fn check(&self) -> Admission {
        let mut inner = self.inner.lock().expect("lock síncrono");
        match inner.state {
            CircuitState::Closed => Admission::Allowed,
            CircuitState::Open => {
                let until = inner.open_until.unwrap_or_else(Instant::now);
                if Instant::now() >= until {
                    inner.state = CircuitState::HalfOpen;
                    inner.half_open_probes_in_flight = 1;
                    Admission::Allowed
                } else {
                    Admission::Blocked {
                        retry_after: until.saturating_duration_since(Instant::now()),
                    }
                }
            }
            CircuitState::HalfOpen => {
                if inner.half_open_probes_in_flight < HALF_OPEN_MAX_PROBES {
                    inner.half_open_probes_in_flight += 1;
                    Admission::Allowed
                } else {
                    Admission::Blocked {
                        retry_after: HALF_OPEN_RETRY_INTERVAL,
                    }
                }
            }
        }
    }

    /// SPEC §7.7 regra 6: sucesso consistente fecha o circuito. Um único
    /// sucesso já fecha — não exigimos uma sequência, para não atrasar a
    /// recuperação além do necessário.
    pub fn record_success(&self) {
        let mut inner = self.inner.lock().expect("lock síncrono");
        inner.state = CircuitState::Closed;
        inner.open_until = None;
        inner.consecutive_transient_failures = 0;
        inner.half_open_probes_in_flight = 0;
        inner.backoff_attempt = 0;
    }

    /// `429` (ou equivalente): abre imediatamente. `retry_after` explícito
    /// é respeitado à risca (SPEC §7.3); na ausência dele, aplica backoff
    /// exponencial com jitter (regra de segurança do PRD §7.3: "retry não é
    /// um loop imediato").
    pub fn record_rate_limited(&self, retry_after: Option<Duration>) {
        let mut inner = self.inner.lock().expect("lock síncrono");
        inner.backoff_attempt += 1;
        let delay = retry_after.unwrap_or_else(|| backoff_with_jitter(inner.backoff_attempt));
        inner.state = CircuitState::Open;
        inner.open_until = Some(Instant::now() + delay);
        inner.half_open_probes_in_flight = 0;
    }

    /// `503`/timeout: só abre após `TRANSIENT_FAILURE_THRESHOLD` falhas
    /// consecutivas no escopo.
    pub fn record_transient_failure(&self, retry_after: Option<Duration>) {
        let mut inner = self.inner.lock().expect("lock síncrono");
        inner.consecutive_transient_failures += 1;
        if matches!(inner.state, CircuitState::HalfOpen) {
            // Uma sonda falhou em HalfOpen — volta a abrir imediatamente,
            // sem esperar o limiar de novo.
            inner.backoff_attempt += 1;
            let delay = retry_after.unwrap_or_else(|| backoff_with_jitter(inner.backoff_attempt));
            inner.state = CircuitState::Open;
            inner.open_until = Some(Instant::now() + delay);
            inner.half_open_probes_in_flight = 0;
        } else if inner.consecutive_transient_failures >= TRANSIENT_FAILURE_THRESHOLD {
            inner.backoff_attempt += 1;
            let delay = retry_after.unwrap_or_else(|| backoff_with_jitter(inner.backoff_attempt));
            inner.state = CircuitState::Open;
            inner.open_until = Some(Instant::now() + delay);
        }
    }
}

fn backoff_with_jitter(attempt: u32) -> Duration {
    let base = Duration::from_secs(1 << attempt.min(6).max(0)).min(MAX_BACKOFF);
    let jitter_fraction: f64 = rand::thread_rng().gen_range(0.5..1.5);
    base.mul_f64(jitter_fraction)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limited_opens_immediately_with_explicit_retry_after() {
        let breaker = CircuitBreaker::new();
        breaker.record_rate_limited(Some(Duration::from_secs(60)));
        assert_eq!(breaker.state(), CircuitState::Open);
        match breaker.check() {
            Admission::Blocked { retry_after } => assert!(retry_after > Duration::from_secs(50)),
            Admission::Allowed => panic!("deveria estar bloqueado"),
        }
    }

    #[test]
    fn transient_failure_does_not_open_before_threshold() {
        let breaker = CircuitBreaker::new();
        breaker.record_transient_failure(None);
        breaker.record_transient_failure(None);
        assert_eq!(breaker.state(), CircuitState::Closed);
        breaker.record_transient_failure(None);
        assert_eq!(breaker.state(), CircuitState::Open);
    }

    #[test]
    fn success_closes_circuit_and_resets_counters() {
        let breaker = CircuitBreaker::new();
        breaker.record_transient_failure(None);
        breaker.record_transient_failure(None);
        breaker.record_success();
        assert_eq!(breaker.state(), CircuitState::Closed);
        // O contador de falhas transitórias foi resetado — duas novas
        // falhas não deveriam já abrir o circuito.
        breaker.record_transient_failure(None);
        breaker.record_transient_failure(None);
        assert_eq!(breaker.state(), CircuitState::Closed);
    }

    #[test]
    fn half_open_allows_single_probe_then_blocks_concurrent_ones() {
        let breaker = CircuitBreaker::new();
        breaker.record_rate_limited(Some(Duration::from_millis(1)));
        std::thread::sleep(Duration::from_millis(5));

        // A primeira checagem após o prazo transiciona para HalfOpen e libera 1 sonda.
        assert!(matches!(breaker.check(), Admission::Allowed));
        assert_eq!(breaker.state(), CircuitState::HalfOpen);
        // Uma segunda sonda concorrente deve ser bloqueada.
        assert!(matches!(breaker.check(), Admission::Blocked { .. }));
    }

    #[test]
    fn failed_probe_in_half_open_reopens_circuit() {
        let breaker = CircuitBreaker::new();
        breaker.record_rate_limited(Some(Duration::from_millis(1)));
        std::thread::sleep(Duration::from_millis(5));
        assert!(matches!(breaker.check(), Admission::Allowed));

        breaker.record_transient_failure(Some(Duration::from_secs(30)));
        assert_eq!(breaker.state(), CircuitState::Open);
    }
}
