//! Mutex por chave — garante que só uma chamada real está em andamento por
//! `parent_item_id`/`item_id`, sem exigir que o resultado seja `Clone`
//! (ao contrário de `nexofs_api_governor::Deduplicator`, que compartilha o
//! valor retornado entre esperantes). Quem esperou relê o índice local
//! depois de obter o lock — que já estará atualizado pela chamada que
//! executou de fato (double-checked locking).

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

pub struct PerKeyLock<K> {
    locks: Mutex<HashMap<K, Arc<AsyncMutex<()>>>>,
}

impl<K> Default for PerKeyLock<K> {
    fn default() -> Self {
        Self {
            locks: Mutex::new(HashMap::new()),
        }
    }
}

impl<K: Eq + Hash + Clone> PerKeyLock<K> {
    pub fn new() -> Self {
        Self::default()
    }

    /// As entradas nunca são removidas do mapa — um pequeno custo de
    /// memória por chave já vista, aceitável para chaves de baixa
    /// cardinalidade (pastas, itens abertos). Limpeza periódica fica para
    /// quando a Fase 6 medir que isso pesa em instalações muito grandes.
    pub async fn lock(&self, key: K) -> OwnedMutexGuard<()> {
        let mutex = {
            let mut locks = self.locks.lock().expect("lock síncrono");
            locks.entry(key).or_insert_with(|| Arc::new(AsyncMutex::new(()))).clone()
        };
        mutex.lock_owned().await
    }
}
