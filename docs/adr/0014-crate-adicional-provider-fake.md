# ADR-014 — `nexofs-provider-fake` como crate adicional

**Status:** Aceito (SPEC §3.1 lista o workspace como exemplo, não como lista fechada; requisito de origem: FR-MC-004, tarefa T0-11)

## Contexto

FR-MC-004 exige que a máquina de estados do núcleo seja "testada deterministicamente" contra um "provider simulado" sem rede. A árvore de crates da SPEC §3.1 não lista um crate dedicado para isso, mas o texto do documento ("provider fake" em §28 Etapa 0) deixa claro que ele deve existir como artefato de teste, não apenas como um mock inline dentro de outro crate.

## Decisão

Adicionar `crates/nexofs-provider-fake` como um crate de biblioteca completo (não apenas `#[cfg(test)]` dentro de `nexofs-provider-api`), implementando o trait `CloudProvider` inteiro sobre estado em memória. Isso permite que `nexofs-sync-core`, `nexofs-api-governor` e os testes de integração/escala (SPEC §26) o usem como dev-dependency sem duplicar a implementação em cada crate consumidor.

## Consequências

- É a única adição ao layout de crates da SPEC §3.1; todos os outros 15 crates seguem a lista original.
- Cobertura já validada: upload/download roundtrip, concorrência otimista (`VersionConflict`), replay de delta com cursor "0" vs. `latest_only`, e tombstone de exclusão ocultando o item da listagem.
- Não deve ser publicado nem usado por `nexofsd` em produção — é dependência de teste/desenvolvimento apenas.
