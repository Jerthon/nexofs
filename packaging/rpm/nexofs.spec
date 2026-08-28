Name:           nexofs
Version:        0.1.0
Release:        1%{?dist}
Summary:        Sistema de arquivos multi-nuvem (FUSE)

License:        AGPL-3.0-or-later
URL:            https://github.com/nexofs/nexofs
Source0:        %{name}-%{version}.tar.gz

BuildRequires:  cargo
BuildRequires:  rust
BuildRequires:  systemd-rpm-macros
Requires:       fuse3

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
pacote instala o daemon %{name}d e a CLI administrativa %{name}.

%prep
%autosetup -n %{name}-%{version}

%build
cargo build --release --locked -p nexofsd -p nexofs-cli

%install
install -Dm755 target/release/nexofsd %{buildroot}%{_bindir}/nexofsd
install -Dm755 target/release/nexofs %{buildroot}%{_bindir}/nexofs
install -Dm644 packaging/systemd/nexofsd.service %{buildroot}%{_userunitdir}/nexofsd.service

%files
%{_bindir}/nexofsd
%{_bindir}/nexofs
%{_userunitdir}/nexofsd.service

%post
%systemd_user_post nexofsd.service

%preun
%systemd_user_preun nexofsd.service

%postun
%systemd_user_postun_with_restart nexofsd.service

%changelog
* Thu Aug 27 2026 NexoFS <noreply@nexofs.dev> - 0.1.0-1
- Primeiro pacote RPM (T5-12) — validado com instalação/desinstalação
  reais em Fedora.
