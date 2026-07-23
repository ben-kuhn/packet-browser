use crate::transport::vara::codec::{parse_line, setup_commands, VaraResponse};
use crate::transport::{
    SessionConfig, Transport, TransportConfig, TransportError, TransportEvent, TransportKind,
};
use async_trait::async_trait;
use std::time::Instant;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

async fn read_cmd_ready(stream: &mut TcpStream) -> std::io::Result<()> {
    stream.readable().await
}

const PLACEHOLDER_CALL: &str = "N0CALL";
const OK_TIMEOUT_SECS: u64 = 5;

pub struct VaraTransport {
    cmd: Option<TcpStream>,
    data: Option<TcpStream>,
    cmd_line_buf: String,
}

impl VaraTransport {
    pub fn new() -> Self {
        Self { cmd: None, data: None, cmd_line_buf: String::new() }
    }

    async fn send_cmd(&mut self, line: &str) -> Result<(), TransportError> {
        let stream = self.cmd.as_mut().ok_or(TransportError::NotConnected)?;
        stream.write_all(line.as_bytes()).await?;
        stream.write_all(b"\r").await?;
        stream.flush().await?;
        Ok(())
    }

    async fn read_cmd_line(&mut self, deadline: Instant) -> Result<String, TransportError> {
        // Read a \r or \n terminated line from self.cmd, retrying WouldBlock
        // until deadline. Return the trimmed line.
        loop {
            if let Some(pos) = self.cmd_line_buf.find(|c: char| c == '\r' || c == '\n') {
                let mut line: String = self.cmd_line_buf.drain(..=pos).collect();
                // Also strip any trailing \n if we split on \r
                if let Some('\n') = self.cmd_line_buf.chars().next() {
                    self.cmd_line_buf.remove(0);
                }
                line.truncate(line.trim_end_matches(|c: char| c == '\r' || c == '\n').len());
                return Ok(line);
            }
            if Instant::now() >= deadline {
                return Err(TransportError::Timeout);
            }
            let stream = self.cmd.as_mut().ok_or(TransportError::NotConnected)?;
            let mut chunk = [0u8; 512];
            let n = tokio::time::timeout(
                std::time::Duration::from_millis(500),
                tokio::io::AsyncReadExt::read(stream, &mut chunk),
            )
            .await
            .map_err(|_| TransportError::Timeout)??;
            if n == 0 { return Err(TransportError::NotConnected); }
            self.cmd_line_buf
                .push_str(&String::from_utf8_lossy(&chunk[..n]));
        }
    }

    async fn await_ok(&mut self) -> Result<(), TransportError> {
        let deadline = Instant::now() + std::time::Duration::from_secs(OK_TIMEOUT_SECS);
        let line = self.read_cmd_line(deadline).await?;
        match parse_line(&line) {
            VaraResponse::Ok => Ok(()),
            VaraResponse::Missing(field) => Err(TransportError::ModemError(format!("MISSING {field}"))),
            other => Err(TransportError::ModemError(format!("expected OK, got {other:?}"))),
        }
    }
}

#[async_trait]
impl Transport for VaraTransport {
    async fn connect_modem(
        &mut self,
        cfg: &TransportConfig,
    ) -> Result<(), TransportError> {
        if !matches!(cfg.kind, TransportKind::VaraFm | TransportKind::VaraHf) {
            return Err(TransportError::ModemError(
                "VaraTransport called with non-VARA TransportKind".into(),
            ));
        }
        let cmd = TcpStream::connect((&*cfg.vara.cmd_host, cfg.vara.cmd_port)).await?;
        let data = TcpStream::connect((&*cfg.vara.data_host, cfg.vara.data_port)).await?;
        self.cmd = Some(cmd);
        self.data = Some(data);

        for cmd_line in setup_commands(PLACEHOLDER_CALL, cfg.vara.mode, cfg.vara.bandwidth) {
            self.send_cmd(&cmd_line).await?;
            self.await_ok().await?;
        }
        Ok(())
    }

    async fn disconnect_modem(&mut self) -> Result<(), TransportError> {
        self.cmd = None;
        self.data = None;
        self.cmd_line_buf.clear();
        Ok(())
    }

    async fn open_session(
        &mut self,
        cfg: &SessionConfig,
    ) -> Result<(), TransportError> {
        // Re-issue MYCALL with the operator's callsign now that we know it.
        self.send_cmd(&format!("MYCALL {}", cfg.local_callsign)).await?;
        self.await_ok().await?;

        // Request the connection.
        self.send_cmd(&format!(
            "CONNECT {} {}",
            cfg.local_callsign, cfg.remote_callsign
        ))
        .await?;

        // Accept PENDING then CONNECTED, or fail on DISCONNECTED / BUSY DETECTED.
        let connect_deadline = Instant::now() + std::time::Duration::from_secs(60);
        loop {
            let line = self.read_cmd_line(connect_deadline).await?;
            match parse_line(&line) {
                VaraResponse::Pending => continue,
                VaraResponse::Connected { .. } => return Ok(()),
                VaraResponse::Disconnected => {
                    return Err(TransportError::SessionRejected(
                        "vara: link dropped during CONNECT".into(),
                    ));
                }
                VaraResponse::BusyDetected => {
                    return Err(TransportError::SessionRejected("channel busy".into()));
                }
                VaraResponse::Unknown(s) => {
                    tracing::debug!(response = %s, "ignoring VARA cmd during CONNECT");
                    continue;
                }
                other => {
                    return Err(TransportError::ModemError(format!(
                        "unexpected during CONNECT: {other:?}"
                    )));
                }
            }
        }
    }

    async fn close_session(&mut self) -> Result<(), TransportError> {
        self.send_cmd("DISCONNECT").await?;
        // Drain up to 3s waiting for DISCONNECTED confirmation.
        let deadline = Instant::now() + std::time::Duration::from_secs(3);
        loop {
            match self.read_cmd_line(deadline).await {
                Ok(line) if matches!(parse_line(&line), VaraResponse::Disconnected) => {
                    return Ok(());
                }
                Ok(_) => continue,
                Err(TransportError::Timeout) => return Ok(()),
                Err(e) => return Err(e),
            }
        }
    }

    async fn send(&mut self, data: &[u8]) -> Result<(), TransportError> {
        let stream = self.data.as_mut().ok_or(TransportError::NotConnected)?;
        stream.write_all(data).await?;
        stream.flush().await?;
        Ok(())
    }

    async fn recv(
        &mut self,
        deadline: Instant,
    ) -> Result<TransportEvent, TransportError> {
        loop {
            if Instant::now() >= deadline {
                return Err(TransportError::Timeout);
            }
            let data = self.data.as_mut().ok_or(TransportError::NotConnected)?;
            let cmd = self.cmd.as_mut().ok_or(TransportError::NotConnected)?;
            let mut data_chunk = [0u8; 4096];
            tokio::select! {
                // Data-port readable → return the bytes.
                n = tokio::io::AsyncReadExt::read(data, &mut data_chunk) => {
                    let n = n?;
                    if n == 0 {
                        return Ok(TransportEvent::Disconnected {
                            reason: "data port closed".into(),
                        });
                    }
                    return Ok(TransportEvent::Data(data_chunk[..n].to_vec()));
                }
                // Command-port readable → parse a line.
                _ = read_cmd_ready(cmd) => {
                    let line_deadline = Instant::now()
                        + std::time::Duration::from_millis(100);
                    let line = self.read_cmd_line(line_deadline).await?;
                    match parse_line(&line) {
                        VaraResponse::Disconnected => {
                            return Ok(TransportEvent::Disconnected {
                                reason: "vara modem reports disconnect".into(),
                            });
                        }
                        other => {
                            tracing::debug!(?other, "VARA cmd line during recv");
                            continue;
                        }
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                    continue;
                }
            }
        }
    }

    fn port_query_supported(&self) -> bool { false }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{
        AgwpeParams, TransportConfig, TransportKind, VaraBandwidth, VaraMode, VaraParams,
    };
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    async fn read_until_cr(reader: &mut (impl tokio::io::AsyncRead + Unpin), out: &mut String) {
        use tokio::io::AsyncReadExt;
        let mut byte = [0u8; 1];
        loop {
            let n = reader.read(&mut byte).await.unwrap();
            if n == 0 { return; }
            if byte[0] == b'\r' { return; }
            out.push(byte[0] as char);
        }
    }

    async fn mock_ports() -> (u16, u16, tokio::task::JoinHandle<Vec<String>>) {
        let cmd_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let data_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let cmd_port = cmd_listener.local_addr().unwrap().port();
        let data_port = data_listener.local_addr().unwrap().port();

        let handle = tokio::spawn(async move {
            let (mut cmd_sock, _) = cmd_listener.accept().await.unwrap();
            let (_data_sock, _) = data_listener.accept().await.unwrap();
            let mut lines = Vec::new();
            let (mut r, mut w) = cmd_sock.split();
            for _ in 0..4 {
                let mut line = String::new();
                read_until_cr(&mut r, &mut line).await;
                lines.push(line);
                w.write_all(b"OK\r").await.unwrap();
            }
            lines
        });

        (cmd_port, data_port, handle)
    }

    fn session_cfg() -> SessionConfig {
        SessionConfig {
            local_callsign: "W1TEST".into(),
            remote_callsign: "N0CALL-8".into(),
            bpq_command: String::new(),
            skip_bpq_app: true,
            agwpe_port: 0,
        }
    }

    #[tokio::test]
    async fn connect_modem_sends_expected_setup_commands() {
        let (cmd_port, data_port, mock) = mock_ports().await;
        let mut vara = VaraTransport::new();
        let cfg = TransportConfig {
            kind: TransportKind::VaraFm,
            agwpe: AgwpeParams { host: "unused".into(), port: 0 },
            vara: VaraParams {
                cmd_host: "127.0.0.1".into(),
                cmd_port,
                data_host: "127.0.0.1".into(),
                data_port,
                mode: VaraMode::Fm,
                bandwidth: VaraBandwidth::VWide,
            },
            local_callsign: "W1TEST".into(),
        };
        vara.connect_modem(&cfg).await.unwrap();

        let lines = mock.await.unwrap();
        assert_eq!(lines, vec![
            "MYCALL ".to_string() + "N0CALL",
            // The line above intentionally uses the placeholder local
            // callsign for this task; open_session (Task 7) will re-issue
            // MYCALL when the operator's callsign is known.
            "LISTEN OFF".to_string(),
            "COMPRESSION OFF".to_string(),
            "VWIDE".to_string(),
        ]);
    }

    #[tokio::test]
    async fn open_session_sends_connect_and_reports_success_on_connected_line() {
        let cmd_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let data_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let cmd_port = cmd_listener.local_addr().unwrap().port();
        let data_port = data_listener.local_addr().unwrap().port();

        let mock = tokio::spawn(async move {
            let (mut cmd_sock, _) = cmd_listener.accept().await.unwrap();
            let (_data_sock, _) = data_listener.accept().await.unwrap();
            let (mut r, mut w) = cmd_sock.split();
            // Ack the four setup commands.
            for _ in 0..4 {
                let mut line = String::new();
                read_until_cr(&mut r, &mut line).await;
                w.write_all(b"OK\r").await.unwrap();
            }
            // Ack the MYCALL re-issue in open_session.
            let mut line = String::new();
            read_until_cr(&mut r, &mut line).await;
            assert_eq!(line, "MYCALL W1TEST");
            w.write_all(b"OK\r").await.unwrap();
            // Ack CONNECT with PENDING then CONNECTED.
            let mut line = String::new();
            read_until_cr(&mut r, &mut line).await;
            assert_eq!(line, "CONNECT W1TEST N0CALL-8");
            w.write_all(b"PENDING\r").await.unwrap();
            w.write_all(b"CONNECTED W1TEST N0CALL-8\r").await.unwrap();
        });

        let mut vara = VaraTransport::new();
        let cfg = TransportConfig {
            kind: TransportKind::VaraFm,
            agwpe: AgwpeParams { host: "unused".into(), port: 0 },
            vara: VaraParams {
                cmd_host: "127.0.0.1".into(),
                cmd_port,
                data_host: "127.0.0.1".into(),
                data_port,
                mode: VaraMode::Fm,
                bandwidth: VaraBandwidth::VWide,
            },
            local_callsign: "W1TEST".into(),
        };
        vara.connect_modem(&cfg).await.unwrap();
        let session = session_cfg();
        vara.open_session(&session).await.unwrap();
        mock.await.unwrap();
    }

    #[tokio::test]
    async fn recv_translates_disconnected_command_line_to_transport_event() {
        let cmd_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let data_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let cmd_port = cmd_listener.local_addr().unwrap().port();
        let data_port = data_listener.local_addr().unwrap().port();

        let mock = tokio::spawn(async move {
            let (mut cmd_sock, _) = cmd_listener.accept().await.unwrap();
            let (_data_sock, _) = data_listener.accept().await.unwrap();
            let (mut r, mut w) = cmd_sock.split();
            // Ack four setup commands.
            for _ in 0..4 {
                let mut line = String::new();
                read_until_cr(&mut r, &mut line).await;
                w.write_all(b"OK\r").await.unwrap();
            }
            // Ack MYCALL re-issue.
            let mut line = String::new();
            read_until_cr(&mut r, &mut line).await;
            w.write_all(b"OK\r").await.unwrap();
            // Ack CONNECT with CONNECTED.
            let mut line = String::new();
            read_until_cr(&mut r, &mut line).await;
            w.write_all(b"CONNECTED W1TEST N0CALL-8\r").await.unwrap();
            // Now emit DISCONNECTED to trigger recv().
            w.write_all(b"DISCONNECTED\r").await.unwrap();
        });

        let mut vara = VaraTransport::new();
        let cfg = TransportConfig {
            kind: TransportKind::VaraFm,
            agwpe: AgwpeParams { host: "unused".into(), port: 0 },
            vara: VaraParams {
                cmd_host: "127.0.0.1".into(),
                cmd_port,
                data_host: "127.0.0.1".into(),
                data_port,
                mode: VaraMode::Fm,
                bandwidth: VaraBandwidth::VWide,
            },
            local_callsign: "W1TEST".into(),
        };
        vara.connect_modem(&cfg).await.unwrap();
        vara.open_session(&session_cfg()).await.unwrap();

        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        let event = vara.recv(deadline).await.unwrap();
        match event {
            TransportEvent::Disconnected { reason } => {
                assert!(reason.contains("disconnect"), "unexpected reason: {reason}");
            }
            other => panic!("expected Disconnected, got {other:?}"),
        }
        mock.await.unwrap();
    }

    #[tokio::test]
    async fn send_writes_bytes_on_data_port() {
        let cmd_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let data_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let cmd_port = cmd_listener.local_addr().unwrap().port();
        let data_port = data_listener.local_addr().unwrap().port();

        let mock = tokio::spawn(async move {
            let (mut cmd_sock, _) = cmd_listener.accept().await.unwrap();
            let (mut data_sock, _) = data_listener.accept().await.unwrap();
            let (mut r, mut w) = cmd_sock.split();
            // Ack four setup commands.
            for _ in 0..4 {
                let mut line = String::new();
                read_until_cr(&mut r, &mut line).await;
                w.write_all(b"OK\r").await.unwrap();
            }
            // Ack MYCALL re-issue.
            let mut line = String::new();
            read_until_cr(&mut r, &mut line).await;
            w.write_all(b"OK\r").await.unwrap();
            // Ack CONNECT.
            let mut line = String::new();
            read_until_cr(&mut r, &mut line).await;
            w.write_all(b"CONNECTED W1TEST N0CALL-8\r").await.unwrap();

            // Read exactly the bytes sent via vara.send().
            let mut buf = vec![0u8; 6];
            tokio::io::AsyncReadExt::read_exact(&mut data_sock, &mut buf).await.unwrap();
            buf
        });

        let mut vara = VaraTransport::new();
        let cfg = TransportConfig {
            kind: TransportKind::VaraFm,
            agwpe: AgwpeParams { host: "unused".into(), port: 0 },
            vara: VaraParams {
                cmd_host: "127.0.0.1".into(),
                cmd_port,
                data_host: "127.0.0.1".into(),
                data_port,
                mode: VaraMode::Fm,
                bandwidth: VaraBandwidth::VWide,
            },
            local_callsign: "W1TEST".into(),
        };
        vara.connect_modem(&cfg).await.unwrap();
        vara.open_session(&session_cfg()).await.unwrap();
        vara.send(b"GET /\n").await.unwrap();

        let received = mock.await.unwrap();
        assert_eq!(received, b"GET /\n");
    }

    #[tokio::test]
    async fn close_session_sends_disconnect_and_drains_confirmation() {
        let cmd_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let data_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let cmd_port = cmd_listener.local_addr().unwrap().port();
        let data_port = data_listener.local_addr().unwrap().port();

        let mock = tokio::spawn(async move {
            let (mut cmd_sock, _) = cmd_listener.accept().await.unwrap();
            let (_data_sock, _) = data_listener.accept().await.unwrap();
            let (mut r, mut w) = cmd_sock.split();
            // Ack four setup commands.
            for _ in 0..4 {
                let mut line = String::new();
                read_until_cr(&mut r, &mut line).await;
                w.write_all(b"OK\r").await.unwrap();
            }
            // Ack MYCALL re-issue.
            let mut line = String::new();
            read_until_cr(&mut r, &mut line).await;
            w.write_all(b"OK\r").await.unwrap();
            // Ack CONNECT.
            let mut line = String::new();
            read_until_cr(&mut r, &mut line).await;
            w.write_all(b"CONNECTED W1TEST N0CALL-8\r").await.unwrap();
            // Read DISCONNECT and send DISCONNECTED back once.
            let mut line = String::new();
            read_until_cr(&mut r, &mut line).await;
            assert_eq!(line, "DISCONNECT");
            w.write_all(b"DISCONNECTED\r").await.unwrap();
        });

        let mut vara = VaraTransport::new();
        let cfg = TransportConfig {
            kind: TransportKind::VaraFm,
            agwpe: AgwpeParams { host: "unused".into(), port: 0 },
            vara: VaraParams {
                cmd_host: "127.0.0.1".into(),
                cmd_port,
                data_host: "127.0.0.1".into(),
                data_port,
                mode: VaraMode::Fm,
                bandwidth: VaraBandwidth::VWide,
            },
            local_callsign: "W1TEST".into(),
        };
        vara.connect_modem(&cfg).await.unwrap();
        vara.open_session(&session_cfg()).await.unwrap();
        vara.close_session().await.unwrap();
        mock.await.unwrap();
    }
}
