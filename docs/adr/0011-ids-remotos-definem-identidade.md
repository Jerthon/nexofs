# ADR-011 — IDs remotos, não caminhos, definem identidade

**Status:** Aceito (SPEC §8.3, §29; implementado em T0-02, `nexofs_domain::inode`)

## Contexto

Se o inode fosse derivado do caminho, um `rename` ou `move` invalidaria handles abertos, quebraria referências do kernel e obrigaria aplicações a perder contexto sobre um arquivo só porque ele mudou de nome ou de pasta (FR-FS-004).

## Decisão

O inode é derivado de forma estável a partir de `(provider_id, account_id, namespace_id, remote_item_id)` — ou de um UUID local para itens ainda sem contrapartida remota — nunca do caminho. Implementado em `nexofs_domain::inode::stable_inode`, com testes confirmando que a mesma identidade produz sempre o mesmo inode e que um rename simulado não o altera.

## Consequências

- Colisões de hash (baixa probabilidade, mas possíveis com um hash de 64 bits) são resolvidas via a tabela persistente `inode_map` (SPEC §10.3), não pela função de hash sozinha.
- Reconstrução de caminho para exibição é feita a partir das relações pai-filho no índice, não o inverso (FR-IDX-005) — cachear caminhos derivados é uma otimização de leitura, nunca a fonte de identidade.
