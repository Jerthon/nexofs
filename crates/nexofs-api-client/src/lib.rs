//! T5-09/SPEC §20.2 — cliente HTTP/1.1 minimalista sobre o Unix Domain
//! Socket da API local. Não puxamos um cliente HTTP genérico (reqwest/
//! hyper-client) só para isto: é sempre localhost, sem TLS, sem redirect,
//! sem keep-alive entre chamadas — o mesmo padrão já usado pelos testes de
//! integração de `nexofs-local-api` é suficiente e evita depender de uma
//! pilha inteira de cliente HTTP para uma dúzia de chamadas simples.
//!
//! Compartilhado por `nexofs-cli` e `nexofs-desktop` (backend Tauri) —
//! ambos são clientes finos da mesma API local, sem lógica própria de
//! sincronização (ADR-005).

use anyhow::{bail, Context, Result};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

pub struct ApiClient {
    socket_path: std::path::PathBuf,
}

impl ApiClient {
    pub fn new(socket_path: std::path::PathBuf) -> Self {
        Self { socket_path }
    }

    pub async fn get(&self, path: &str) -> Result<Value> {
        self.request("GET", path, None).await
    }

    pub async fn post(&self, path: &str, body: Option<Value>) -> Result<Value> {
        self.request("POST", path, body).await
    }

    pub async fn delete(&self, path: &str) -> Result<Value> {
        self.request("DELETE", path, None).await
    }

    async fn request(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
        let mut stream = UnixStream::connect(&self.socket_path).await.with_context(|| {
            format!(
                "não foi possível conectar em {} — o daemon nexofsd está rodando?",
                self.socket_path.display()
            )
        })?;

        let body_bytes = body.as_ref().map(serde_json::to_vec).transpose()?.unwrap_or_default();
        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n",
            body_bytes.len()
        );
        if !body_bytes.is_empty() {
            request.push_str("Content-Type: application/json\r\n");
        }
        request.push_str("\r\n");

        stream.write_all(request.as_bytes()).await?;
        stream.write_all(&body_bytes).await?;

        let mut response = Vec::new();
        stream.read_to_end(&mut response).await?;
        let response = String::from_utf8_lossy(&response);

        let mut parts = response.splitn(2, "\r\n\r\n");
        let head = parts.next().unwrap_or_default();
        let raw_body = parts.next().unwrap_or_default();

        let status: u16 = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse().ok())
            .unwrap_or(0);

        let json: Value = if raw_body.trim().is_empty() { Value::Null } else { serde_json::from_str(raw_body).unwrap_or(Value::Null) };

        if !(200..300).contains(&status) {
            let message = json.get("error").and_then(|e| e.get("message")).and_then(|m| m.as_str()).unwrap_or(raw_body).to_string();
            bail!("{method} {path} -> HTTP {status}: {message}");
        }

        Ok(json)
    }

    /// `GET /v1/events` (SPEC §20.4) nunca fecha sozinho — chamador decide
    /// quando parar de ler (aqui, só até o processo ser interrompido).
    ///
    /// `on_connected` dispara uma única vez, assim que o cabeçalho HTTP
    /// completo chega (prova de que o daemon aceitou a conexão e já
    /// assinou o barramento de eventos do lado dele) — o ponto certo para
    /// quem chama refazer um snapshot completo do estado, cobrindo tanto
    /// "app abriu antes do daemon estar de pé" quanto uma reconexão depois
    /// de o daemon cair: nenhum dos dois casos gera um `SyncEvent`
    /// específico para reagir, só a prova de conexão em si.
    ///
    /// A resposta chega com `Transfer-Encoding: chunked` (axum sempre
    /// envia SSE assim); isto não faz o decode formal de chunked, só ignora
    /// as linhas de framing (tamanho em hex, linhas vazias) porque nenhuma
    /// delas começa com `data:` — suficiente para um cliente que só fala
    /// com o próprio `nexofsd`, sem pretensão de ser um cliente HTTP geral.
    pub async fn stream_events(&self, mut on_connected: impl FnMut(), mut on_line: impl FnMut(&str)) -> Result<()> {
        let mut stream = UnixStream::connect(&self.socket_path).await.with_context(|| {
            format!("não foi possível conectar em {} — o daemon nexofsd está rodando?", self.socket_path.display())
        })?;
        stream.write_all(b"GET /v1/events HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\n\r\n").await?;

        let mut buf = vec![0u8; 4096];
        let mut pending = String::new();
        let mut headers_skipped = false;
        loop {
            let n = stream.read(&mut buf).await?;
            if n == 0 {
                bail!("o daemon fechou o stream de eventos");
            }
            pending.push_str(&String::from_utf8_lossy(&buf[..n]));

            if !headers_skipped {
                let Some(pos) = pending.find("\r\n\r\n") else { continue };
                pending = pending[pos + 4..].to_string();
                headers_skipped = true;
                on_connected();
            }

            while let Some(pos) = pending.find('\n') {
                let line = pending[..pos].trim_end_matches('\r').to_string();
                pending = pending[pos + 1..].to_string();
                if let Some(data) = line.strip_prefix("data:") {
                    on_line(data.trim());
                }
            }
        }
    }
}
