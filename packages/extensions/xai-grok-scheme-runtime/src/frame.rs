//! Length-prefixed frame transport: 4-byte little-endian u32 length header
//! followed by a UTF-8 payload. Mirrored byte-for-byte by the kernel's
//! `read-frame` / `write-frame` in `kernel/runtime.ss`.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Hard cap for one frame payload (either direction).
pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// Write one frame. Payload must be under [`MAX_FRAME_BYTES`].
pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, payload: &str) -> std::io::Result<()> {
    let bytes = payload.as_bytes();
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame too large: {} bytes", bytes.len()),
        ));
    }
    let len = (bytes.len() as u32).to_le_bytes();
    w.write_all(&len).await?;
    w.write_all(bytes).await?;
    w.flush().await
}

/// Read one frame. Returns `Ok(None)` on clean EOF at a frame boundary.
///
/// Errors on: EOF mid-frame, oversized length header, invalid UTF-8.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Option<String>> {
    let mut header = [0u8; 4];
    match r.read_exact(&mut header).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(header) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("incoming frame too large: {len} bytes"),
        ));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    String::from_utf8(buf)
        .map(Some)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "frame is not valid UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_roundtrip_over_duplex() {
        // Buffer must exceed the largest test frame: reader runs after writer.
        let (mut a, mut b) = tokio::io::duplex(256 * 1024);
        for payload in ["", "(hello 1)", "unicode 中文 🦀", &"x".repeat(100_000)] {
            write_frame(&mut a, payload).await.unwrap();
            let got = read_frame(&mut b).await.unwrap().unwrap();
            assert_eq!(got, payload);
        }
        drop(a);
        assert!(read_frame(&mut b).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn oversized_and_truncated_frames_error() {
        // Oversized length header.
        let (mut a, mut b) = tokio::io::duplex(1024);
        let bad_len = ((MAX_FRAME_BYTES + 1) as u32).to_le_bytes();
        tokio::io::AsyncWriteExt::write_all(&mut a, &bad_len).await.unwrap();
        assert!(read_frame(&mut b).await.is_err());

        // EOF mid-frame.
        let (mut a, mut b) = tokio::io::duplex(1024);
        let len = 100u32.to_le_bytes();
        tokio::io::AsyncWriteExt::write_all(&mut a, &len).await.unwrap();
        tokio::io::AsyncWriteExt::write_all(&mut a, b"short").await.unwrap();
        drop(a);
        assert!(read_frame(&mut b).await.is_err());

        // Invalid UTF-8 payload.
        let (mut a, mut b) = tokio::io::duplex(1024);
        let len = 2u32.to_le_bytes();
        tokio::io::AsyncWriteExt::write_all(&mut a, &len).await.unwrap();
        tokio::io::AsyncWriteExt::write_all(&mut a, &[0xff, 0xfe]).await.unwrap();
        assert!(read_frame(&mut b).await.is_err());
    }
}
