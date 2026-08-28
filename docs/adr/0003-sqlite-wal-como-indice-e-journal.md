# ADR-003 — SQLite WAL como índice e journal local

**Status:** Aceito (PRD §10.1, SPEC §10, §29; spike T0-06)

## Contexto

O índice local precisa suportar leitura concorrente com um único processo de escrita (o daemon), sobreviver a `kill -9` sem corrupção lógica (NFR-REL-002) e escalar a milhões de itens sem exigir um servidor de banco separado.

## Decisão

SQLite em modo `journal_mode=WAL`, acessado via a crate `rusqlite` (feature `bundled`, para não depender da versão de `libsqlite3` do sistema). Escritas são serializadas por uma única thread dedicada (`nexofs-metadata-store`); leituras abrem conexões próprias, que o modo WAL permite executar concorrentemente sem bloquear o escritor.

## Alternativas consideradas

`sqlx` foi avaliado por oferecer pool assíncrono nativo, mas seu modelo de conexões múltiplas conflita com o requisito de "escritor único" (SPEC §10.4) — seria necessário construir a mesma serialização por cima de qualquer driver. `rusqlite` síncrono, isolado em uma thread dedicada com uma fila de comandos, atende o requisito diretamente e com menos código.

## Consequências

- Leituras usam uma conexão nova por chamada dentro de `spawn_blocking`; um pool de conexões reutilizáveis fica para quando um benchmark de escala (Fase 6, T6-01/T6-05) mostrar que o custo de abertura pesa.
- `PRAGMA busy_timeout=5000` cobre esperas pontuais de checkpoint; não substitui a serialização do escritor.
