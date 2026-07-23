use packet_browser_client::transport::vara::VaraTransport;
use packet_browser_client::transport::{
    AgwpeParams, SessionConfig, Transport, TransportConfig, TransportEvent, TransportKind,
    VaraBandwidth, VaraMode, VaraParams,
};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Read a CR-terminated line from a TcpStream half (VARA protocol uses \r, not \n).
async fn read_cr_line(reader: &mut (impl AsyncReadExt + Unpin)) -> String {
    let mut line = String::new();
    let mut byte = [0u8; 1];
    loop {
        let n = reader.read(&mut byte).await.unwrap();
        if n == 0 {
            break;
        }
        if byte[0] == b'\r' {
            break;
        }
        line.push(byte[0] as char);
    }
    line
}

#[tokio::test]
async fn vara_lifecycle_connect_send_recv_reconnect() {
    let cmd_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let data_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let cmd_port = cmd_listener.local_addr().unwrap().port();
    let data_port = data_listener.local_addr().unwrap().port();

    let mock = tokio::spawn(async move {
        let (mut cmd_sock, _) = cmd_listener.accept().await.unwrap();
        let (mut data_sock, _) = data_listener.accept().await.unwrap();

        // Setup phase: MYCALL / LISTEN OFF / COMPRESSION OFF / VWIDE.
        let (mut r, mut w) = cmd_sock.split();
        for _ in 0..4 {
            let _line = read_cr_line(&mut r).await;
            w.write_all(b"OK\r").await.unwrap();
        }

        // open_session #1: MYCALL re-issue, then CONNECT.
        let line = read_cr_line(&mut r).await;
        assert_eq!(line.trim(), "MYCALL W1TEST");
        w.write_all(b"OK\r").await.unwrap();

        let line = read_cr_line(&mut r).await;
        assert_eq!(line.trim(), "CONNECT W1TEST N0CALL-8");
        w.write_all(b"PENDING\r").await.unwrap();
        w.write_all(b"CONNECTED W1TEST N0CALL-8\r").await.unwrap();

        // Data phase.
        let mut got = [0u8; 5];
        data_sock.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"HELLO");
        data_sock.write_all(b"WORLD").await.unwrap();

        // close_session.
        let line = read_cr_line(&mut r).await;
        assert_eq!(line.trim(), "DISCONNECT");
        w.write_all(b"DISCONNECTED\r").await.unwrap();

        // open_session #2 (simulate a reconnect).
        let line = read_cr_line(&mut r).await;
        assert_eq!(line.trim(), "MYCALL W1TEST");
        w.write_all(b"OK\r").await.unwrap();
        let line = read_cr_line(&mut r).await;
        assert_eq!(line.trim(), "CONNECT W1TEST N0CALL-8");
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
    let session = SessionConfig {
        local_callsign: "W1TEST".into(),
        remote_callsign: "N0CALL-8".into(),
        bpq_command: String::new(),
        skip_bpq_app: true,
        agwpe_port: 0,
    };

    vara.connect_modem(&cfg).await.unwrap();
    vara.open_session(&session).await.unwrap();
    vara.send(b"HELLO").await.unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    match vara.recv(deadline).await.unwrap() {
        TransportEvent::Data(bytes) => assert_eq!(bytes, b"WORLD"),
        other => panic!("expected Data, got {other:?}"),
    }

    vara.close_session().await.unwrap();
    vara.open_session(&session).await.unwrap();

    mock.await.unwrap();
}
