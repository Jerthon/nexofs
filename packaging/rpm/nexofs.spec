Name:           nexofs
Version:        0.1.1
Release:        1%{?dist}
Summary:        Sistema de arquivos multi-nuvem (FUSE)

License:        AGPL-3.0-or-later
URL:            https://github.com/nexofs/nexofs
Source0:        %{name}-%{version}.tar.gz

BuildRequires:  cargo
BuildRequires:  rust
BuildRequires:  systemd-rpm-macros
BuildRequires:  nodejs
BuildRequires:  npm
BuildRequires:  webkit2gtk4.1-devel
BuildRequires:  gtk3-devel
BuildRequires:  libappindicator-gtk3-devel
BuildRequires:  openssl-devel
Requires:       fuse3
Requires:       webkit2gtk4.1
Requires:       gtk3
Requires:       libappindicator-gtk3

# T5-12: build local via `cargo build --release`, reaproveitando o cache
# de ~/.cargo/registry já populado nesta máquina — suficiente para validar
# instalação/desinstalação real em Fedora (o objetivo desta tarefa). Um
# pacote pronto para revisão oficial do Fedora (Packaging Guidelines para
# Rust) exigiria vendorizar as dependências via %generate_buildrequires/
# cargo_prep para funcionar em mock/Koji sem acesso à rede — isso fica
# registrado como pendência em NexoFS_TASKS_v1.0.md, não é necessário para
# o critério de aceite desta fase (instalação/desinstalação local limpa).
%global debug_package %{nil}

%description
NexoFS monta contas de nuvem como um sistema de
arquivos FUSE local, com sincronização incremental, cache de conteúdo,
exclusões estilo .gitignore e resolução de conflitos completa. Este
pacote instala o daemon %{name}d, a CLI administrativa %{name} e a
interface gráfica (%{name}-desktop) — tudo num único pacote.

%prep
%autosetup -n %{name}-%{version}

%build
cargo build --release --locked -p nexofsd -p nexofs-cli
# T7-06: mesma interface gráfica que "cargo tauri build" empacotaria
# sozinha num segundo pacote (NexoFS) dependendo deste aqui — o usuário
# pediu um único instalador, então o binário do Tauri entra direto neste
# spec. `cargo build` funciona sem a CLI `cargo-tauri`, desde que o
# `dist/` do frontend já exista (só o `npm run build`/vite é obrigatório).
cd desktop && npm ci && npm run build && cd ..
cargo build --release --locked --features custom-protocol --manifest-path desktop/src-tauri/Cargo.toml

%install
install -Dm755 target/release/nexofsd %{buildroot}%{_bindir}/nexofsd
install -Dm755 target/release/nexofs %{buildroot}%{_bindir}/nexofs
install -Dm644 packaging/systemd/nexofsd.service %{buildroot}%{_userunitdir}/nexofsd.service
install -Dm755 desktop/src-tauri/target/release/nexofs-desktop %{buildroot}%{_bindir}/nexofs-desktop
install -Dm644 packaging/desktop/NexoFS.desktop %{buildroot}%{_datadir}/applications/NexoFS.desktop
install -Dm644 desktop/src-tauri/icons/32x32.png %{buildroot}%{_datadir}/icons/hicolor/32x32/apps/nexofs-desktop.png
install -Dm644 desktop/src-tauri/icons/128x128.png %{buildroot}%{_datadir}/icons/hicolor/128x128/apps/nexofs-desktop.png
install -Dm644 desktop/src-tauri/icons/128x128@2x.png %{buildroot}%{_datadir}/icons/hicolor/256x256@2/apps/nexofs-desktop.png
install -Dm644 desktop/src-tauri/icons/icon.png %{buildroot}%{_datadir}/icons/hicolor/512x512/apps/nexofs-desktop.png

%files
%{_bindir}/nexofsd
%{_bindir}/nexofs
%{_bindir}/nexofs-desktop
%{_userunitdir}/nexofsd.service
%{_datadir}/applications/NexoFS.desktop
%{_datadir}/icons/hicolor/32x32/apps/nexofs-desktop.png
%{_datadir}/icons/hicolor/128x128/apps/nexofs-desktop.png
%{_datadir}/icons/hicolor/256x256@2/apps/nexofs-desktop.png
%{_datadir}/icons/hicolor/512x512/apps/nexofs-desktop.png

%post
%systemd_user_post nexofsd.service

%preun
%systemd_user_preun nexofsd.service

%postun
%systemd_user_postun_with_restart nexofsd.service

%changelog
* Sat Aug 29 2026 NexoFS <noreply@nexofs.dev> - 0.1.1-1
- Corrige o access token do provedor nunca ser renovado depois do mount
  inicial: expirava em ~1h e travava o journal inteiro em
  WaitingRetry/FailedPermanent até um restart manual. Agora
  AuthenticationRequired tenta renovar via refresh token na hora.
- GET /v1/operations (e "nexofs operations") agora inclui operações que
  falharam de vez (FailedPermanent) com a mensagem de erro do provedor —
  antes elas desapareciam do journal e só existiam no log do daemon.
- Corrige upload de arquivo de 0 bytes para o OneDrive retornando
  "411 Length Required" — Content-Length agora é declarado explicitamente
  no PUT em vez de depender da heurística do cliente HTTP.
- Interface desktop: tela de Operações ganhou coluna "Arquivo" (nome do
  item, antes só tipo/estado apareciam) e indicadores de total/falhas; a
  coluna "Arquivo" de Conflitos ganhou largura máxima com quebra de linha
  para não espremer as outras colunas.
- A interface gráfica (nexofs-desktop) passa a ser instalada por este
  mesmo pacote — antes só existia como um segundo pacote separado (NexoFS)
  gerado por "cargo tauri build", dependendo deste aqui.
- Corrige nexofs-desktop abrindo com "Could not connect to localhost:
  Connection refused" quando instalado — o binário empacotado via
  "cargo build" direto (sem a CLI cargo-tauri) carregava o servidor de
  dev do Vite em vez dos assets embutidos, por faltar a feature
  "custom-protocol" do Tauri.
- Corrige o daemon esgotando os file descriptors do processo ("Too many
  open files") sob uma rajada de operações do journal — cada leitura do
  metadata store abria uma conexão SQLite própria sem limite de
  concorrência algum; agora há um teto de 32 leituras concorrentes.
  Sintoma visível: a partir do esgotamento, até o servidor HTTP local
  parava de aceitar conexões, e a tela de Operações da interface ficava
  vazia mesmo com sincronização acontecendo normalmente.
- Corrige GET /v1/operations (e "nexofs operations") ficando lento a
  ponto de nunca responder em contas com milhares de operações
  pendentes — resolver o nome/caminho de cada operação reabria a árvore
  de pastas inteira do zero por operação. A rota agora é paginada
  (`limit`/`offset`, contagens via COUNT(*)) e aceita filtros por
  estado, tipo e nome do arquivo (`state`/`operation_type`/`search`).
- Implementa a resolução de conflitos do tipo "item local colidiu com um
  item remoto homônimo antes de ser enviado" (antes retornava HTTP 500
  "resolução ainda não implementada") — manter local/os dois renomeia o
  item local para liberar o nome; manter remoto descarta o item local
  ainda não enviado.
- Interface desktop: a tela de Operações ganhou busca por nome de
  arquivo, filtros de tipo/estado e paginação (antes carregava a fila
  inteira de uma vez, o que travava a tela em contas grandes); clicar no
  indicador de falhas filtra direto para elas. Operações e Conflitos
  também deixam de perder os dados e reiniciar o carregamento ao trocar
  de aba e voltar.
- Corrige um upload que ficava tentando de novo para sempre com "File
  exists" depois que o daemon era reiniciado (ou morria) no meio de um
  envio — sobrava um arquivo de snapshot órfão em `partial/` que
  bloqueava toda tentativa futura com o mesmo erro. O snapshot agora é
  substituído em vez de falhar quando já existe.
* Thu Aug 27 2026 NexoFS <noreply@nexofs.dev> - 0.1.0-1
- Primeiro pacote RPM (T5-12) — validado com instalação/desinstalação
  reais em Fedora.
