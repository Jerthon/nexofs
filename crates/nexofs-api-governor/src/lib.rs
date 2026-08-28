//! Provider API Governor (SPEC §7). Nenhum adaptador DEVE chamar um
//! provedor remoto sem passar por aqui (FR-API-001, API-001).

mod circuit_breaker;
mod dedup;
mod governor;
mod priority_queue;
mod scope;
mod token_bucket;

pub use circuit_breaker::{Admission, CircuitBreaker, CircuitState};
pub use dedup::Deduplicator;
pub use governor::{GovernorPermit, ProviderApiGovernor, ScopeMetrics};
pub use priority_queue::PriorityQueue;
pub use scope::{OperationClass, Priority, RateScope};
pub use token_bucket::TokenBucket;
