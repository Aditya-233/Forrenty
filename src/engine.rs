use anyhow::{Context, Result};
use std::net::SocketAddr;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

static ENGINE_INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Ensures that a SOCKS5 DPI bypass engine is active on 127.0.0.1:1080.
/// If port 1080 is already open, it reuses the active instance.
/// If not, it spawns an integrated Tokio asynchronous DPI bypass proxy in the background.
pub async fn ensure_engine_running() -> Result<()> {
    if is_local_port_open(1080).await {
        crate::log_info!("DPI bypass engine active on 127.0.0.1:1080 (reusing instance)");
        return Ok(());
    }

    if ENGINE_INITIALIZED.swap(true, Ordering::SeqCst) {
        return Ok(());
    }

    crate::log_info!("Starting integrated Rust SOCKS5 DPI bypass engine on 127.0.0.1:1080...");
    tokio::spawn(async {
        if let Err(e) = run_socks5_server("127.0.0.1", 1080).await {
            crate::log_error!("Embedded SOCKS5 DPI engine exited with error: {:#}", e);
        }
    });

    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if is_local_port_open(1080).await {
            crate::log_info!("Integrated Rust SOCKS5 DPI bypass engine is online!");
            return Ok(());
        }
    }

    crate::log_warn!("Embedded SOCKS5 engine took longer than expected to bind port 1080");
    Ok(())
}

async fn is_local_port_open(port: u16) -> bool {
    TcpStream::connect(SocketAddr::from(([127, 0, 0, 1], port)))
        .await
        .is_ok()
}

async fn run_socks5_server(listen_ip: &str, port: u16) -> Result<()> {
    let addr = format!("{listen_ip}:{port}");
    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("failed to bind embedded SOCKS5 engine on {addr}"))?;

    loop {
        let (stream, peer_addr) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                crate::log_warn!("Embedded SOCKS5 engine accept error: {e}");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };

        tokio::spawn(async move {
            let _ = stream.set_nodelay(true);
            if let Err(e) = handle_socks5_client(stream, peer_addr).await {
                crate::log_debug!("Embedded SOCKS5 client {} error: {e}", peer_addr);
            }
        });
    }
}

async fn handle_socks5_client(mut client: TcpStream, _peer: SocketAddr) -> Result<()> {
    let mut ver_methods = [0u8; 2];
    client.read_exact(&mut ver_methods).await?;
    if ver_methods[0] != 5 {
        anyhow::bail!("unsupported SOCKS version: {}", ver_methods[0]);
    }
    let num_methods = ver_methods[1] as usize;
    let mut methods = vec![0u8; num_methods];
    client.read_exact(&mut methods).await?;

    // Respond: VER 5, METHOD 0 (no authentication)
    client.write_all(&[5, 0]).await?;
    client.flush().await?;

    // Read connection request: VER, CMD, RSV, ATYP
    let mut header = [0u8; 4];
    client.read_exact(&mut header).await?;
    if header[0] != 5 || header[1] != 1 {
        // CMD 1 = CONNECT
        client.write_all(&[5, 7, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
        anyhow::bail!("only SOCKS5 CONNECT is supported");
    }

    let target_host = match header[3] {
        1 => {
            // IPv4
            let mut ip = [0u8; 4];
            client.read_exact(&mut ip).await?;
            std::net::Ipv4Addr::from(ip).to_string()
        }
        3 => {
            // Domain name
            let mut len = [0u8; 1];
            client.read_exact(&mut len).await?;
            let mut domain = vec![0u8; len[0] as usize];
            client.read_exact(&mut domain).await?;
            String::from_utf8(domain).context("invalid UTF-8 domain")?
        }
        4 => {
            // IPv6
            let mut ip = [0u8; 16];
            client.read_exact(&mut ip).await?;
            std::net::Ipv6Addr::from(ip).to_string()
        }
        other => {
            client.write_all(&[5, 8, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
            anyhow::bail!("unsupported ATYP: {other}");
        }
    };

    let mut port_buf = [0u8; 2];
    client.read_exact(&mut port_buf).await?;
    let target_port = u16::from_be_bytes(port_buf);

    let target_addr = format!("{target_host}:{target_port}");
    let mut server = match TcpStream::connect(&target_addr).await {
        Ok(s) => s,
        Err(e) => {
            let _ = client.write_all(&[5, 4, 0, 1, 0, 0, 0, 0, 0, 0]).await;
            return Err(e).with_context(|| format!("failed to connect to {target_addr}"));
        }
    };
    let _ = server.set_nodelay(true);

    // Reply SOCKS5 success: VER 5, REP 0, RSV 0, ATYP 1, BND.ADDR 0.0.0.0, BND.PORT 0
    client.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
    client.flush().await?;

    // Perform TCP Out-Of-Band (OOB) Desynchronization on the first client packet
    let mut first_buf = vec![0u8; 16384];
    let n = client.read(&mut first_buf).await?;
    if n == 0 {
        return Ok(());
    }
    first_buf.truncate(n);

    // Check if this is a TLS ClientHello (0x16 0x03 ...)
    let is_tls = first_buf.len() > 3 && first_buf[0] == 0x16 && first_buf[1] == 0x03;
    let is_http = first_buf.starts_with(b"GET ")
        || first_buf.starts_with(b"POST ")
        || first_buf.starts_with(b"HEAD ");

    if is_tls && first_buf.len() > 1 {
        let raw_fd = server.as_raw_fd();
        // Exact -o 1 TCP OOB desync:
        // Byte 0 is first_buf[0] (0x16), byte 1 is replaced with dummy 'a'.
        // Sending 2 bytes with MSG_OOB causes TCP Urgent pointer to point to 'a' at seq 1.
        let fake_two = [first_buf[0], b'a'];
        let sent = unsafe {
            libc::send(
                raw_fd,
                fake_two.as_ptr().cast(),
                2,
                libc::MSG_OOB,
            )
        };

        if sent > 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            server.write_all(&first_buf[1..]).await?;
            server.flush().await?;
        } else {
            server.write_all(&first_buf).await?;
            server.flush().await?;
        }
    } else if is_http && first_buf.len() > 1 {
        let raw_fd = server.as_raw_fd();
        let host_pos = first_buf
            .windows(6)
            .position(|w| w.eq_ignore_ascii_case(b"\nhost:") || w.eq_ignore_ascii_case(b"\rhost:"))
            .map(|p| p + 6)
            .unwrap_or(1);

        let pos = host_pos.min(first_buf.len() - 1);
        let mut fake_buf = first_buf[..=pos].to_vec();
        fake_buf[pos] = b'a';
        let sent = unsafe {
            libc::send(
                raw_fd,
                fake_buf.as_ptr().cast(),
                fake_buf.len(),
                libc::MSG_OOB,
            )
        };

        if sent > 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            server.write_all(&first_buf[pos..]).await?;
            server.flush().await?;
        } else {
            server.write_all(&first_buf).await?;
            server.flush().await?;
        }
    } else {
        server.write_all(&first_buf).await?;
        server.flush().await?;
    }

    // Proxy bidirectional traffic continuously
    let (mut client_read, mut client_write) = client.into_split();
    let (mut server_read, mut server_write) = server.into_split();

    let client_to_server = tokio::io::copy(&mut client_read, &mut server_write);
    let server_to_client = tokio::io::copy(&mut server_read, &mut client_write);

    tokio::select! {
        _ = client_to_server => {},
        _ = server_to_client => {},
    }

    Ok(())
}
