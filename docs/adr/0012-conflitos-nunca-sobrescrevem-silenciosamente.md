# ADR-012 — Conflitos nunca sobrescrevem silenciosamente

**Status:** Aceito (PRD §6.10, SPEC §18, §29)

## Contexto

Uma alteração remota concorrente sobrescrita sem detecção é uma das falhas mais citadas em soluções concorrentes (PRD §2.1) e o principal risco de "perda de dados" listado no PRD (§20, "Conflitos complexos").

## Decisão

Toda mutação destrutiva (upload, move, delete) condiciona a operação à `base_remote_version` registrada quando o item foi lido/modificado localmente (FR-UP-006). Divergência entre a versão base e a versão remota atual, com o item local `dirty`, gera um `Conflict` — nunca um overwrite. Ambas as versões (local e remota) são preservadas até resolução explícita (FR-CON-002), e um conflito adiado permanece protegido de eviction (FR-CON-005).

## Consequências

- O `nexofs-provider-fake` já implementa e testa esse comportamento (`optimistic_concurrency_detects_conflict`): uma escrita com `base_remote_version` desatualizada retorna `ProviderErrorKind::VersionConflict` em vez de aceitar a sobrescrita.
- Toda resolução de conflito (`KeepLocal`, `KeepRemote`, `KeepBoth`, `SaveLocalElsewhere`) deve ser idempotente — reexecutar a mesma decisão após uma falha não pode duplicar o efeito (PRD §17.4).
