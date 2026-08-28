//! Inicialização de `tracing`. SPEC §23.1.
//!
//! `NEXOFS_LOG_FORMAT=json` produz logs estruturados (integração externa);
//! o padrão é texto legível, adequado a `journalctl --user -u nexofsd`.
//! O nível é controlado por `RUST_LOG` (padrão: `info`).

use tracing_subscriber::EnvFilter;

pub fn init() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let use_json = std::env::var("NEXOFS_LOG_FORMAT")
        .map(|v| v.eq_ignore_ascii_case("json"))
        .unwrap_or(false);

    let subscriber = tracing_subscriber::fmt().with_env_filter(filter);
    if use_json {
        subscriber.json().init();
    } else {
        subscriber.init();
    }
}
