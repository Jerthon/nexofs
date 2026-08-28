# ADR-010 — `fsync` garante durabilidade local, não remota

**Status:** Aceito (SPEC §8.4, §29)

## Contexto

Aplicações chamam `fsync` esperando uma garantia POSIX de durabilidade — mas essa chamada é síncrona do ponto de vista da aplicação, enquanto o upload para a nuvem é assíncrono e pode levar minutos (arquivo grande, throttling, offline). Prometer durabilidade remota em `fsync` obrigaria a bloquear a aplicação chamadora até a confirmação do provedor, o que é inaceitável para UX e viola a operação offline (FR-OFF-002).

## Decisão

`fsync` garante apenas: conteúdo gravado no armazenamento local, metadados e operação persistidos no SQLite/journal, arquivo recuperável após reinício. Não garante que o upload remoto tenha terminado.

## Consequências

- A perda de dados em caso de falha do processo é coberta pelo journal (durabilidade local), não pela conclusão do upload.
- Uma extensão futura pode oferecer uma operação explícita "aguardar sincronização remota" para quem precisa dessa garantia mais forte (ex.: scripts de CI que gravam e imediatamente esperam propagação).
- A UI deve deixar claro, quando relevante, que "salvo" (local) e "sincronizado" (remoto) são estados distintos.
