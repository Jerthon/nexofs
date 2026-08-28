use std::io::Cursor;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, ReadBuf};

/// `AsyncRead` sobre um buffer em memória — o conteúdo do `FakeProvider`
/// nunca toca disco ou rede, mantendo os testes determinísticos.
pub struct InMemoryReader(Cursor<Vec<u8>>);

impl InMemoryReader {
    pub fn new(data: Vec<u8>) -> Self {
        Self(Cursor::new(data))
    }
}

impl AsyncRead for InMemoryReader {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let read = std::io::Read::read(&mut this.0, buf.initialize_unfilled())?;
        buf.advance(read);
        Poll::Ready(Ok(()))
    }
}
