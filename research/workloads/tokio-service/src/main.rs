use tokio::{io::{AsyncReadExt, AsyncWriteExt}, net::TcpListener};
use std::time::Duration;

fn main() {
    let multi = std::env::args().nth(1).as_deref() == Some("multi");
    let mut builder = if multi { tokio::runtime::Builder::new_multi_thread() } else { tokio::runtime::Builder::new_current_thread() };
    if multi { builder.worker_threads(2); }
    let runtime = builder.enable_all().build().unwrap();
    runtime.block_on(async {
        let listener = TcpListener::bind("127.0.0.1:19400").await.unwrap();
        for _ in 0..64 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 8];
            stream.read_exact(&mut request).await.unwrap();
            let value = u64::from_le_bytes(request);
            tokio::time::sleep(Duration::from_millis(1)).await;
            stream.write_all(&(value.wrapping_add(1)).to_le_bytes()).await.unwrap();
        }
    });
}
