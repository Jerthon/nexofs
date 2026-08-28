# ADR-004 — Tauri 2 para interface desktop

**Status:** Aceito (PRD §10.1, SPEC §2.2.2, §29)

## Contexto

A UI precisa rodar em GNOME e KDE Plasma (Wayland e X11), ser leve o suficiente para não comprometer a meta de RSS do sistema, e nunca acessar o SQLite ou os tokens diretamente (SPEC §2.2.2) — apenas falar com o daemon pela API local.

## Decisão

Interface desktop construída com Tauri 2 (WebView do sistema + backend Rust fino) e TypeScript/React no frontend.

## Alternativas consideradas

| Opção | Linguagem | Por que não |
|---|---|---|
| Electron | JS/TS, Chromium embutido por app | Cada app empacota o próprio Chromium — RSS e tamanho de binário bem maiores; é a comparação mais direta com Tauri, e o requisito de RSS baixo (PRD §15.1) pesa contra. |
| GTK4 nativo (`gtk4-rs`/Relm4) | Rust, widgets nativos GTK | Mais leve ainda que Tauri, mas foge do visual nativo no KDE Plasma — um dos dois ambientes-alvo (SPEC §25.1) — e a UI declarativa em Rust puro tem curva mais dura que React para as telas de tabela/formulário desta fase (filas, conflitos). |
| Qt/QML (`cxx-qt` ou similar) | C++/QML + bindings Rust | Resolveria o KDE mas erra no GNOME — o problema inverso do GTK; o projeto precisa dos dois desktops. |
| egui/iced | Rust puro, sem WebView | 100% Rust, sem dependência de WebView do sistema — mas nenhum dos dois é visualmente nativo em nenhuma distro, e a maturidade para telas ricas (progresso de transferência, resolução de conflito) é menor que a de um frontend web. |
| App web local + navegador do usuário | Qualquer stack web | Sem janela própria nem integração de bandeja via StatusNotifierItem (FR-UI-003, requisito desta fase) — descartado cedo. |

Tauri fica no meio-termo: mais leve que Electron (reaproveita o WebView do sistema — WebKitGTK no Linux — em vez de empacotar Chromium), e evita escolher entre GNOME e KDE como GTK/Qt nativos forçariam, ao custo de depender de um WebView compatível instalado na distro (ver Consequências).

## Consequências

- Reaproveita WebView do sistema em vez de empacotar um Chromium inteiro (ao contrário de Electron), mantendo o binário e o RSS menores.
- Depende de um WebView compatível disponível na distro (SPEC §25.3) — parte da matriz de compatibilidade a validar na Fase 5.
- O processo Tauri é um cliente da API local do daemon, nunca um processo com estado próprio de sincronização — reforça ADR-005.
