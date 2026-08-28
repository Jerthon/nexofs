# ADR-009 — Conteúdo ignorado usa Local-Only Overlay

**Status:** Aceito (PRD §8, SPEC §11.4/§17, §29)

## Contexto

Diretórios como `node_modules`, `vendor` e `target` precisam existir no caminho do projeto (ferramentas de build esperam isso), mas não devem gerar journal, hash nem chamadas remotas — e não podem ser tratados como cache descartável, porque são dados que o usuário espera encontrar depois de reiniciar (PRD §8.1).

## Decisão

Conteúdo coberto por uma regra de exclusão ativa (`.nexofsignore`, perfil ou política) é persistido no Local-Only Overlay — uma camada gravável e persistente, separada do Remote Content Cache, fora da política de eviction LRU (FR-LOC-001). O ponto de montagem apresenta uma visão mesclada única (remoto + cache + overlay), sem que a aplicação que o acessa precise saber em qual camada um item vive.

## Consequências

- A limpeza de cache (`FR-CACHE-004`) nunca remove conteúdo do overlay.
- Colisão de nome entre um item local-only e um item remoto exige decisão explícita do usuário (FR-LOC-003), nunca resolução silenciosa.
- Mudar uma regra de exclusão sobre conteúdo já existente (em qualquer direção) exige estimativa de custo e confirmação antes de enfileirar qualquer operação (FR-LOC-005/006).
