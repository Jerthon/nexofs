# ADR-006 — Provider-neutral core

**Status:** Aceito (PRD §9, SPEC §5, §29; implementado em T0-10/T0-11)

## Contexto

OneDrive é o primeiro adaptador, não o modelo do sistema (PRD §9). Se o núcleo (`nexofs-sync-core`) referenciasse tipos do SDK do Microsoft Graph, adicionar Google Drive ou Dropbox exigiria reescrever a máquina de estados em vez de apenas implementar um novo adaptador (FR-MC-001, FR-MC-005).

## Decisão

`nexofs-provider-api` define o contrato `CloudProvider` e todos os tipos de troca (`RemoteItem`, `ProviderCapabilities`, `ProviderErrorKind`, etc.) usando apenas conceitos genéricos. `nexofs-domain` e `nexofs-sync-core` não têm, e não podem ter, uma dependência de crate de nenhum SDK de provedor específico. `nexofs-provider-fake` implementa o mesmo contrato sem rede, permitindo testar o núcleo inteiro de forma determinística (FR-MC-004) — já validado com 5 testes cobrindo upload/download, concorrência otimista, delta com/sem `latest_only` e tombstones.

## Consequências

- Cada adaptador (`nexofs-provider-onedrive`, e futuramente Google Drive/Dropbox) converte erros e capacidades específicas para a taxonomia neutra na borda — o núcleo nunca inspeciona um código HTTP do Graph.
- `ProviderCapabilities` decide em runtime se delta, ranges, hash ou upload resumível estão disponíveis (FR-MC-002) — o núcleo não assume nenhum deles.
- Um segundo provedor (Fase 7) deve rodar a mesma suíte de contrato usada pelo fake, sem tocar `nexofs-fuse` ou o journal (PRD §17.5).
