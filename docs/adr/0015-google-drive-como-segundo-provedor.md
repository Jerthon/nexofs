# ADR-015 — Google Drive como segundo provedor (Fase 7)

**Status:** Aceito (PRD §21 questão aberta "segundo provedor: Google Drive ou Dropbox"; requisito de origem: T7-01)

## Contexto

O PRD deixava em aberto qual seria o segundo provedor de nuvem a implementar depois do OneDrive, listando Google Drive e Dropbox como candidatos (PRD §21, SPEC §31). A escolha final não dependia de uma análise técnica prévia registrada aqui — o usuário decidiu diretamente por Google Drive ao pedir a funcionalidade.

## Decisão

Implementar `nexofs-provider-googledrive` sobre a Google Drive API v3, seguindo a mesma estrutura de módulos do adaptador OneDrive (`config`/`dto`/`mapping`/normalização de erro/`lib.rs` com o `impl CloudProvider`).

## Consequências e diferenças reais em relação ao OneDrive

Documentadas também nos pontos do código onde aparecem (`nexofs-provider-googledrive/src/lib.rs`):

- **`client_id`/`client_secret` pertencem ao app NexoFS, não ao usuário final.** Igual ao app registration da Microsoft usado pelo OneDrive (ADR-013), é um único OAuth client "Desktop" registrado uma vez pelo projeto e embutido no binário via `option_env!` em tempo de compilação (`config.rs`) — o instalador já sai pronto, sem nenhuma configuração pós-instalação. Uma variável de ambiente em runtime com o mesmo nome (`NEXOFS_GOOGLEDRIVE_CLIENT_ID`/`_SECRET`) continua funcionando como override, para quem quiser usar seu próprio projeto Google Cloud (ex: empresa com cota própria) sem recompilar.
- **`client_secret` exigido mesmo para app "Desktop".** O endpoint de token do Google pede esse parâmetro mesmo para clientes nativos/instalados. Isso não torna o valor um segredo de servidor: RFC 8252 trata clientes nativos como públicos por definição, e o próprio Google documenta que o `client_secret` de um client tipo "Desktop" não é confidencial — por isso pode ser embutido no binário distribuído com segurança, do mesmo jeito que o `client_id` do OneDrive.
- **Nomes duplicados são permitidos numa pasta.** O Drive não tem um "falhar se já existir" nativo como o `conflictBehavior: fail` do Graph — `create_directory`/`upload` replicam essa semântica no cliente, verificando colisão de nome antes de escrever.
- **Sem precondition HTTP documentado para escrita condicional.** Ao contrário do `If-Match` do Graph (atômico no servidor), o controle otimista de versão aqui é uma checagem "ler-depois-escrever" (`check_version_precondition`) — mais fraca (uma corrida estreita entre o GET e a escrita ainda é tecnicamente possível), mas detecta a esmagadora maioria das edições concorrentes que T3-07/FR-UP-006 existem para pegar.
- **Mover exige duas chamadas.** `addParents`/`removeParents` precisam do pai atual, que o Graph não exige (só manda o novo `parentReference.id` numa única `PATCH`) — `capabilities.atomic_move = false` reflete isso.
- **Changes API não tem modo "desde o início".** Só existe "a partir de agora" (`changes.getStartPageToken`) — sem efeito prático porque `nexofs-sync-core` nunca chama `create_change_cursor` com `latest_only: false` de qualquer forma (a indexação é sempre preguiçosa, FR-IDX-002/003).
- **Upload sempre via sessão resumível**, mesmo para arquivos pequenos — simplificação deliberada para não implementar upload multipart à mão.
- **Arquivos nativos do Google Workspace (Docs/Sheets/Slides) não são suportados** nesta entrega — não têm conteúdo binário próprio, exigiriam `files.export`. Mesmo escopo de "SharePoint fica para depois" já aceito para o OneDrive.
- **Nunca validado contra uma conta Google real.** Não há projeto Google Cloud/credenciais disponíveis no ambiente onde isto foi construído — o adaptador segue a documentação pública da API v3, mas, como aconteceu com o OneDrive (SPEC de bugs reais só encontrados em validação ao vivo, `NexoFS_TASKS_v1.0.md` Fase 3), é esperado precisar de ajustes na primeira validação real.

## Trabalho estrutural que esta decisão puxou (T7-02/T7-03)

- `CloudProvider` ganhou um novo método de trait, `refresh_via_refresh_token` — antes um método inerente só de `OneDriveProvider` (`refresh_access_token`), generalizado para o trait porque `nexofsd` precisa retomar contas de qualquer provedor sem conhecer o tipo concreto por trás do `dyn CloudProvider`.
- `nexofsd` deixou de assumir um único provedor fixo: `bootstrap::build_provider_registry()` monta um `HashMap<String, Arc<dyn CloudProvider>>` a partir de `accounts.provider_id`; todas as funções de montagem/adicionar/remontar conta recebem o provedor certo por essa tabela, nunca um tipo concreto hardcoded.
- `HashAlgorithm` ganhou a variante `Md5` (o hash que a Drive API expõe) — campo puramente descritivo, não consumido por nenhuma lógica hoje.
