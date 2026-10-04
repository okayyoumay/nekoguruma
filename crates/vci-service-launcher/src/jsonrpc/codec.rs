use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub trait MessageCodec {
    fn read_message<R: AsyncRead + Unpin>(
        &self,
        reader: &mut R,
    ) -> impl Future<Output = io::Result<Vec<u8>>>;
    fn write_message<W: AsyncWrite + Unpin>(
        &self,
        writer: &mut W,
        body: &[u8],
    ) -> impl Future<Output = io::Result<()>>;
}

pub struct OneLinerCodec;

impl MessageCodec for OneLinerCodec {
    async fn read_message<R: AsyncRead + Unpin>(&self, reader: &mut R) -> io::Result<Vec<u8>> {
        let mut bytes: Vec<u8> = Vec::new();
        let mut buf = [0u8; 1];
        loop {
            match reader.read_exact(&mut buf).await {
                Ok(_) => match buf[0] {
                    b'\r' => {
                        let mut next = [0u8; 1];
                        match reader.read_exact(&mut next).await {
                            Ok(_) => {}
                            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {}
                            Err(e) => return Err(e),
                        }
                        break;
                    }
                    b'\n' => break,
                    b => bytes.push(b),
                },
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    if bytes.is_empty() {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "stdin reached EOF",
                        ));
                    }
                    break;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(bytes)
    }

    async fn write_message<W: AsyncWrite + Unpin>(
        &self,
        writer: &mut W,
        body: &[u8],
    ) -> io::Result<()> {
        writer.write_all(body).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await
    }
}
