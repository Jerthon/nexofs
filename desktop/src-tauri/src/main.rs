// Ponto de entrada fino — a lógica de verdade mora em `lib.rs` (convenção
// do Tauri 2, necessária para builds mobile reaproveitarem o mesmo lib).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // WebKitGTK trava com "Error 71 dispatching to Wayland display" em
    // compositores/drivers que não suportam o renderer DMA-BUF de vídeo
    // dele — precisa ser setado antes do primeiro init do GTK/WebKit.
    std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    nexofs_desktop_lib::run();
}
