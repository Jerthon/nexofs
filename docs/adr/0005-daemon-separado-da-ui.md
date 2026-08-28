# ADR-005 — Daemon separado da UI

**Status:** Aceito (PRD §2.2/§3.4, SPEC §2.2, §29)

## Contexto

Uploads, downloads e a montagem FUSE precisam continuar funcionando com a UI fechada (FR-UP-002: "daemon conclui fila autonomamente"). Se a lógica de sincronização vivesse no processo da UI, fechar a janela interromperia a sincronização.

## Decisão

`nexofsd` é um processo `systemd --user` de longa duração, independente do ciclo de vida de `nexofs-desktop` (Tauri) e de `nexofs-cli`. Ambos os clientes falam com o daemon exclusivamente pela API local (Unix Domain Socket, ADR-016).

## Consequências

- A UI pode ser fechada e reaberta sem afetar filas, montagem ou downloads em andamento.
- O daemon precisa de seu próprio ciclo de vida, tratamento de sinais e reinício supervisionado pelo systemd (`Restart=on-failure`).
- Qualquer estado exibido na UI é sempre uma projeção do estado do daemon via eventos (SPEC §20.4) — a UI nunca é fonte de verdade.
