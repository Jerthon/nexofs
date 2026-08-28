//! Token bucket para controle de rajadas por escopo. SPEC §7.4/§7.8.

use std::sync::Mutex;
use std::time::{Duration, Instant};

struct Bucket {
    tokens: f64,
    capacity: f64,
    refill_per_sec: f64,
    last_refill: Instant,
}

impl Bucket {
    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        self.last_refill = now;
    }
}

/// Um token por chamada. Capacidade = rajada máxima instantânea;
/// `refill_per_sec` = taxa sustentada. Ambos derivam do limite de
/// concorrência do escopo (SPEC §7.8) — não são um controle independente
/// configurado à parte, para não exigir dois conjuntos de números por classe.
pub struct TokenBucket {
    bucket: Mutex<Bucket>,
}

impl TokenBucket {
    pub fn new(capacity: u32, refill_per_sec: f64) -> Self {
        Self {
            bucket: Mutex::new(Bucket {
                tokens: capacity as f64,
                capacity: capacity as f64,
                refill_per_sec,
                last_refill: Instant::now(),
            }),
        }
    }

    /// Bloqueia até um token estar disponível, dormindo exatamente o tempo
    /// necessário (sem polling) quando o bucket está vazio.
    pub async fn acquire(&self) {
        loop {
            let wait = {
                let mut bucket = self.bucket.lock().expect("lock não é mantido durante await");
                bucket.refill();
                if bucket.tokens >= 1.0 {
                    bucket.tokens -= 1.0;
                    None
                } else {
                    let deficit = 1.0 - bucket.tokens;
                    Some(Duration::from_secs_f64(deficit / bucket.refill_per_sec))
                }
            };

            match wait {
                None => return,
                Some(duration) => tokio::time::sleep(duration).await,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn allows_burst_up_to_capacity_then_throttles() {
        let bucket = TokenBucket::new(2, 1.0);

        // Rajada inicial: 2 tokens disponíveis imediatamente.
        let start = tokio::time::Instant::now();
        bucket.acquire().await;
        bucket.acquire().await;
        assert_eq!(tokio::time::Instant::now(), start);

        // Terceiro token exige esperar o refill (~1s a 1 token/s).
        bucket.acquire().await;
        assert!(tokio::time::Instant::now() >= start + Duration::from_millis(900));
    }
}
