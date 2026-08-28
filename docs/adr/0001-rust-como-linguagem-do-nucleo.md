# ADR-001 — Rust como linguagem do daemon e núcleo

**Status:** Aceito (PRD §10.1, SPEC §29)

## Contexto

O daemon precisa rodar continuamente em segundo plano com baixo consumo de CPU/memória (NFR: CPU ociosa < 1%, RSS < 300 MB), implementar um filesystem FUSE de baixa latência e lidar com concorrência (I/O de rede, SQLite, FUSE) sem coletor de lixo pausando operações sensíveis a tempo.

## Decisão

O núcleo, o daemon e os adaptadores de provedor são implementados em Rust, com Tokio como runtime assíncrono.

## Consequências

- Segurança de memória sem GC; performance previsível para FUSE.
- Ecossistema maduro para FUSE (`fuser`), SQLite (`rusqlite`) e HTTP (`reqwest`/`rustls`).
- Curva de aprendizado e tempo de compilação maiores que linguagens gerenciadas — aceito dado o perfil de longa duração do daemon.
