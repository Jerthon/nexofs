# ADR-002 — FUSE 3 para filesystem em espaço de usuário

**Status:** Aceito (PRD §10.1, SPEC §8, §29; spike T0-05)

## Contexto

O NexoFS precisa expor cada conta/namespace como um diretório Linux navegável por Nautilus, Dolphin, editores e terminal, sem exigir um daemon privilegiado (FR-FS-001) e suportando invalidação seletiva de entradas de diretório após delta (FR-REF-006).

## Decisão

Filesystem virtual implementado sobre FUSE 3, usando a crate `fuser` (binding Rust de alto nível para libfuse/FUSE 3, ativamente mantida, com suporte a `notify_inval_entry`/`notify_inval_inode` necessário para invalidação seletiva).

## Consequências

- Montagem no contexto do próprio usuário, sem `CAP_SYS_ADMIN` (com `user_allow_other` desabilitado por padrão).
- Subconjunto de operações POSIX é explícito; `symlink`, hard link e device files retornam `ENOTSUP` no MVP (FR-FS-005).
- Dependência de `libfuse3` no sistema (empacotada via RPM/DEB, SPEC §25.3).
- Se `fuser` mostrar limitações de manutenção durante a Fase 1, a fronteira `nexofs-fuse` isola o impacto de uma eventual troca — nenhum outro crate depende diretamente da API do binding.
- `nexofs-fuse` usa `fuser` com `default-features = false` (sem a feature `libfuse`): nesse modo, no Linux, a crate fala o protocolo FUSE diretamente via syscalls e só precisa do binário `fusermount3` em tempo de execução para obter o descritor de montagem — dispensa `fuse3-devel`/`pkg-config` no ambiente de build (validado em 25/08/2026 em Fedora 44 sem `fuse3-devel` instalado).

