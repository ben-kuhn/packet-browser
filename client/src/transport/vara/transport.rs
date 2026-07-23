use crate::transport::vara::codec::{parse_line, setup_commands, VaraResponse};
use crate::transport::{
    SessionConfig, Transport, TransportConfig, TransportError, TransportEvent, TransportKind,
};
use async_trait::async_trait;
use std::time::Instant;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

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
        stream.write_all(b"\r\n").await?;
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
        _cfg: &SessionConfig,
    ) -> Result<(), TransportError> {
        // Filled in Task 7.
        Err(TransportError::ModemError("open_session not yet implemented".into()))
    }

    async fn close_session(&mut self) -> Result<(), TransportError> {
        // Filled in Task 7.
        Err(TransportError::ModemError("close_session not yet implemented".into()))
    }

    async fn send(&mut self, _data: &[u8]) -> Result<(), TransportError> {
        Err(TransportError::ModemError("send not yet implemented".into()))
    }

    async fn recv(
        &mut self,
        _deadline: Instant,
    ) -> Result<TransportEvent, TransportError> {
        Err(TransportError::ModemError("recv not yet implemented".into()))
    }

    fn port_query_supported(&self) -> bool { false }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{
        AgwpeParams, TransportConfig, TransportKind, VaraBandwidth, VaraMode, VaraParams,
    };
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    async fn mock_ports() -> (u16, u16, tokio::task::JoinHandle<Vec<String>>) {
        let cmd_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let data_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let cmd_port = cmd_listener.local_addr().unwrap().port();
        let data_port = data_listener.local_addr().unwrap().port();

        let handle = tokio::spawn(async move {
            let (mut cmd_sock, _) = cmd_listener.accept().await.unwrap();
            let (_data_sock, _) = data_listener.accept().await.unwrap();
            let mut lines = Vec::new();
            let (r, mut w) = cmd_sock.split();
            let mut reader = BufReader::new(r);
            for _ in 0..4 {
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                lines.push(line.trim().to_string());
                w.write_all(b"OK\r").await.unwrap();
            }
            lines
        });

        (cmd_port, data_port, handle)
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
}
