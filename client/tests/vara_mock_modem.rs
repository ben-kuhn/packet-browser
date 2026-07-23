use packet_browser_client::transport::manager::TransportManager;
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

/// Drive the full `TransportManager → VaraTransport → connect_modem +
/// open_session` happy path end-to-end using a mock modem.  This exercises the
/// manager actor wiring and verifies that `connect_modem` + `open_session` both
/// succeed when a modem is present.  The reviewer identified this as the
/// coverage gap for the VARA transport layer.
///
/// The BPQ handshake requires operator consent; we supply it programmatically
/// by polling `SharedState.pending_consent` from a background task.
#[tokio::test]
async fn transport_manager_vara_connect_open_session_happy_path() {
    use packet_browser_client::config::FileConfig;
    use packet_browser_client::state::{create_shared_state, LockExt};
    use tokio::sync::broadcast;

    let cmd_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let data_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let cmd_port = cmd_listener.local_addr().unwrap().port();
    let data_port = data_listener.local_addr().unwrap().port();

    // Mock modem: ack setup cmds, then MYCALL + CONNECT → CONNECTED.
    // Then acts as a BPQ node over the data port.
    let mock = tokio::spawn(async move {
        let (mut cmd_sock, _) = cmd_listener.accept().await.unwrap();
        let (mut data_sock, _) = data_listener.accept().await.unwrap();
        let (mut r, mut w) = cmd_sock.split();

        // 4 setup commands (MYCALL / LISTEN OFF / COMPRESSION OFF / bandwidth).
        for _ in 0..4 {
            let _line = read_cr_line(&mut r).await;
            w.write_all(b"OK\r").await.unwrap();
        }

        // open_session: MYCALL re-issue.
        let line = read_cr_line(&mut r).await;
        assert_eq!(line.trim(), "MYCALL W1TEST");
        w.write_all(b"OK\r").await.unwrap();

        // open_session: CONNECT.
        let line = read_cr_line(&mut r).await;
        assert_eq!(line.trim(), "CONNECT W1TEST N0CALL-9");
        w.write_all(b"CONNECTED W1TEST N0CALL-9\r").await.unwrap();

        // BPQ handshake over data port.
        // 1. Manager sends BPQ app command ("WEB\n"); eat it.
        let mut buf = vec![0u8; 64];
        let _ = data_sock.read(&mut buf).await;
        // 2. Send callsign prompt so the manager exits its callsign-wait loop.
        data_sock.write_all(b"Enter your callsign: ").await.unwrap();
        // 3. Manager sends its callsign; eat it.
        let _ = data_sock.read(&mut buf).await;
        // 4. Send AGREE disclaimer.
        data_sock
            .write_all(b"All activity is logged. Type AGREE to proceed: ")
            .await
            .unwrap();
        // 5. Manager sends "AGREE\n" after consent is granted; eat it.
        let _ = data_sock.read(&mut buf).await;
        // Keep the socket open long enough for the manager to record Connected.
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    let config = FileConfig::default();
    let shared_state = create_shared_state(config.clone());
    let (log_tx, _rx) = broadcast::channel(16);

    // Auto-consent task: poll shared_state for pending_consent and approve it.
    let consent_state = shared_state.clone();
    let _consent_task = tokio::spawn(async move {
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let tx = {
                let mut s = consent_state.lock_or_poisoned();
                s.pending_consent.take()
            };
            if let Some(tx) = tx {
                let _ = tx.send(true);
                return;
            }
        }
        // If we never saw a pending_consent within 2.5s the test will fail
        // naturally via open_session returning an error.
    });

    let vara_transport: Box<dyn packet_browser_client::transport::Transport> =
        Box::new(VaraTransport::new());
    let manager = TransportManager::spawn(
        vara_transport,
        shared_state,
        log_tx,
        config.connection.response_timeout_secs,
    );

    let transport_cfg = TransportConfig {
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

    // connect_modem must succeed before open_session.
    manager
        .connect_modem(transport_cfg)
        .await
        .expect("TransportManager::connect_modem should succeed with mock modem");

    // open_session drives MYCALL + CONNECT on the VARA transport; the mock
    // responds with CONNECTED then drives the BPQ handshake to completion
    // (with programmatic operator consent from the auto-consent task above).
    manager
        .open_session("N0CALL-9".into(), 0)
        .await
        .expect("TransportManager::open_session should succeed with mock modem");

    mock.await.unwrap();
}
