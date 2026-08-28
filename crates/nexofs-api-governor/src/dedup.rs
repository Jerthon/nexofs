//! Deduplicação de chamadas equivalentes — N solicitações concorrentes com
//! a mesma chave compartilham o mesmo future/resultado (FR-API-003).
//! Exemplos do PRD §7.2: N `readdir` no mesmo namespace → 1 delta em voo;
//! cliques repetidos em "Verificar atualizações" → 1 execução.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex};
use tokio::sync::OnceCell;

pub struct Deduplicator<K, T> {
    in_flight: Mutex<HashMap<K, Arc<OnceCell<T>>>>,
}

impl<K, T> Default for Deduplicator<K, T> {
    fn default() -> Self {
        Self {
            in_flight: Mutex::new(HashMap::new()),
        }
    }
}

impl<K, T> Deduplicator<K, T>
where
    K: Eq + Hash + Clone,
    T: Clone,
{
    pub fn new() -> Self {
        Self::default()
    }

    /// Se já existe uma chamada em voo para `key`, aguarda o resultado dela
    /// em vez de executar `make_call` de novo. A entrada é removida do mapa
    /// assim que a chamada completa — dedup vale apenas para concorrência
    /// real, nunca vira um cache implícito de resultado antigo.
    pub async fn run<F, Fut>(&self, key: K, make_call: F) -> T
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let cell = {
            let mut map = self.in_flight.lock().expect("lock síncrono");
            map.entry(key.clone()).or_insert_with(|| Arc::new(OnceCell::new())).clone()
        };

        let result = cell.get_or_init(make_call).await.clone();

        {
            let mut map = self.in_flight.lock().expect("lock síncrono");
            if let Some(current) = map.get(&key) {
                if Arc::ptr_eq(current, &cell) {
                    map.remove(&key);
                }
            }
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn concurrent_calls_with_same_key_share_one_execution() {
        let dedup: Arc<Deduplicator<&'static str, u32>> = Arc::new(Deduplicator::new());
        let call_count = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..10 {
            let dedup = dedup.clone();
            let call_count = call_count.clone();
            handles.push(tokio::spawn(async move {
                dedup
                    .run("namespace-1", || async move {
                        call_count.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        42
                    })
                    .await
            }));
        }

        for handle in handles {
            assert_eq!(handle.await.unwrap(), 42);
        }
        assert_eq!(call_count.load(Ordering::SeqCst), 1, "10 chamadas concorrentes deveriam virar 1 execução real");
    }

    #[tokio::test]
    async fn sequential_calls_after_completion_run_again() {
        let dedup: Deduplicator<&'static str, u32> = Deduplicator::new();
        let call_count = Arc::new(AtomicUsize::new(0));

        for _ in 0..3 {
            let call_count = call_count.clone();
            dedup
                .run("namespace-1", || async move {
                    call_count.fetch_add(1, Ordering::SeqCst);
                    1
                })
                .await;
        }

        assert_eq!(call_count.load(Ordering::SeqCst), 3, "chamadas sequenciais (não concorrentes) não devem ser deduplicadas");
    }
}
