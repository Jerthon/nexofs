# ADR-007 — Toda API externa passa pelo Governor

**Status:** Aceito (PRD §7, SPEC §7, §29; core implementado em T0-12)

## Contexto

Sem um ponto central de controle, uma indexação em background poderia monopolizar a capacidade necessária para abrir um arquivo interativamente, e picos de chamadas poderiam causar throttling prolongado (429/503) em toda a conta (PRD §7.1).

## Decisão

Nenhum adaptador de provedor pode chamar uma API remota fora do `ProviderApiGovernor` (API-001). O Governor aplica um semáforo por `RateScope` (provider + account + tenant? + namespace + operation_class), de modo que uma classe de operação nunca consome o orçamento de outra — validado em teste (`different_scopes_never_block_each_other`). Token bucket, circuit breaker, backoff/jitter e suporte a `Retry-After` são construídos por cima desse núcleo na Fase 2 (T2-04/T2-05).

## Consequências

- Limites de concorrência iniciais por classe (SPEC §7.8): 1 delta, 4 downloads interativos, 2 uploads, 2 metadados, 1 indexação em background — configuráveis, nunca zero.
- Uma fila de prioridade (`PriorityQueue`) ordena o despacho antes da aquisição do semáforo, respeitando a tabela de prioridades da SPEC §7.5 (download interativo antes de indexação em background, validado em teste).
- Análise estática/testes de integração devem continuar confirmando ausência de bypass conforme novos adaptadores forem adicionados (critério de aceite de FR-API-001).
