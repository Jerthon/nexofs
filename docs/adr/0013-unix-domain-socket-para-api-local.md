# ADR-013 — Unix Domain Socket para API local

**Status:** Aceito (SPEC §2.1/§20.1, §31.3; spike T0-07)

## Contexto

`nexofs-desktop` e `nexofs-cli` precisam falar com `nexofsd` sem acessar SQLite diretamente (SPEC §2.2.2/§2.2.3). A SPEC já lista o socket como transporte assumido, mas registra a escolha entre Unix Domain Socket e D-Bus como questão técnica pendente (§31.3).

## Decisão

Unix Domain Socket em `$XDG_RUNTIME_DIR/nexofs/control.sock`, permissão `0600`, com HTTP/1.1 + JSON por cima (SPEC §20.2) — mantido como transporte definitivo.

## Justificativa sobre D-Bus

D-Bus adicionaria integração mais idiomática com notificações desktop e descoberta de serviço, mas exigiria uma dependência de sessão D-Bus ativa (nem sempre presente em todo ambiente `systemd --user`) e um esquema de introspecção próprio. Um socket Unix com HTTP:
- não depende de um barramento de sessão específico;
- reutiliza ferramentas HTTP padrão (`curl`, bibliotecas cliente) para diagnóstico manual e para o `nexofs-cli`;
- mantém o mesmo modelo de autenticação (permissão de arquivo + `SO_PEERCRED`) descrito em NFR-SEC-006, sem depender das políticas de barramento do D-Bus.

Notificações desktop nativas (SPEC §13.3) continuam via portal/D-Bus especificamente para esse fim — a escolha aqui é só sobre o canal de controle.

## Consequências

- `nexofs-local-api` implementa o servidor HTTP sobre `tokio::net::UnixListener`.
- Validação de UID do peer via `SO_PEERCRED` é obrigatória antes de qualquer resposta que não seja `/v1/status` (NFR-SEC-006).
- Stream de eventos (SPEC §20.4) usa o mesmo socket via SSE ou WebSocket — sem canal separado.
