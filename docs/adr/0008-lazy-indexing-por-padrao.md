# ADR-008 — Lazy indexing por padrão

**Status:** Aceito (PRD §6.3, SPEC §14, §29)

## Contexto

Repositórios com centenas de milhares ou milhões de itens tornam uma enumeração completa na conexão inicial lenta e cara, e a maioria das pastas nunca chega a ser visitada pelo usuário (PRD §2.1).

## Decisão

A raiz é carregada e a montagem fica disponível antes de qualquer indexação ampla (FR-IDX-002). Pastas filhas só são enumeradas quando acessadas via `readdir`/`lookup` (FR-IDX-003). O cursor de mudanças, quando o provedor suporta, é criado "a partir de agora" (`latest_only`) para não forçar um scan histórico completo (FR-IDX-004) — comportamento já coberto por teste no `nexofs-provider-fake`.

## Consequências

- `children_state` (SPEC §10.3, tabela `items`) rastreia se os filhos de uma pasta já foram carregados, evitando reconsultas.
- Full scan só ocorre em recuperação explícita (cursor inválido/expirado) ou por decisão administrativa — nunca como comportamento padrão.
