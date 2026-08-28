import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Configuração mínima recomendada pelo template Tauri 2 + React:
// porta fixa (casa com `devUrl` em src-tauri/tauri.conf.json), ignora
// mudanças em src-tauri/ (o watcher do `cargo tauri dev` já cuida disso,
// duplicar o watch só desperdiça CPU).
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    watch: {
      ignored: ["**/src-tauri/**"],
    },
  },
});
