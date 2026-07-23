## Fix 1

### Files touched
- client/src/proxy.rs

### Test results
80 unit tests passed; 2 vara_mock_modem tests passed; 3 api_connect_transport_dispatch tests passed.

### Commit hash
bf61a59

## Fix 2

### Files touched
- client/src/transport/agwpe.rs

### Test results
80 unit tests passed; 2 vara_mock_modem tests passed; 3 api_connect_transport_dispatch tests passed.

### Commit hash
5613519

## Fix 3

### Files touched
- client/src/transport/manager.rs
- client/src/transport/session.rs

### Test results
80 unit tests passed (including 2 new manager tests: close_session_sets_abort_flag, disconnect_modem_sets_abort_flag); 2 vara_mock_modem tests passed; 3 api_connect_transport_dispatch tests passed.

### Commit hash
185adef

## Fix pass 2

### Fix A — Abort-flag reset race

### Files touched
- client/src/transport/manager.rs (removed reset_abort() from SendRequest and SendRequestWithReconnect arms)
- client/src/transport/session.rs (added is_aborted() guard at top of handle_send_request and handle_send_request_with_reconnect)

### Test commands + outputs
```
nix develop -c cargo test -p packet-browser-client
test result: ok. 80 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.33s
nix develop -c cargo test -p packet-browser-client --test vara_mock_modem
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.41s
nix develop -c cargo test -p packet-browser-client --test api_connect_transport_dispatch
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

### Commit hash
a7ba974

---

### Fix B — Missed connect_modem lock-across-await site

### Files touched
- client/src/proxy.rs (cloned TransportManager out of Mutex before connect_modem await in api_agwpe_status_post)

### Test commands + outputs
```
nix develop -c cargo test -p packet-browser-client
test result: ok. 80 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.32s
nix develop -c cargo test -p packet-browser-client --test vara_mock_modem
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.46s
nix develop -c cargo test -p packet-browser-client --test api_connect_transport_dispatch
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

### Commit hash
6d3388e

---

### Fix C — is_aborted() gap before second handle_send_request in reconnect flow

### Files touched
- client/src/transport/session.rs (added is_aborted() guard after handle_reconnect succeeds, before retry send in handle_send_request_with_reconnect)

### Test commands + outputs
```
nix develop -c cargo test -p packet-browser-client
test result: ok. 80 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.33s
nix develop -c cargo test -p packet-browser-client --test vara_mock_modem
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.41s
nix develop -c cargo test -p packet-browser-client --test api_connect_transport_dispatch
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

### Commit hash
74b98d5

---

### Fix D — Tests exercise real abort preemption via BlockingTransport

### Files touched
- client/src/transport/manager.rs (rewrote close_session_sets_abort_flag and disconnect_modem_sets_abort_flag tests to use BlockingTransport whose recv() sleeps 50ms per call and returns Data([0]); test spawns send_request_with_reconnect, waits 100ms, sets abort_flag, asserts Err(DisconnectedByOperator) within 2s)

### Test commands + outputs
```
nix develop -c cargo test -p packet-browser-client
test result: ok. 80 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.31s
  (includes transport::manager::tests::close_session_sets_abort_flag ... ok)
  (includes transport::manager::tests::disconnect_modem_sets_abort_flag ... ok)
nix develop -c cargo test -p packet-browser-client --test vara_mock_modem
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.46s
nix develop -c cargo test -p packet-browser-client --test api_connect_transport_dispatch
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

### Commit hash
5d47198
