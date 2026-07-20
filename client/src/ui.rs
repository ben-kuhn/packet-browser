/// Escape a string for safe insertion into HTML element text or attribute values
/// (double-quoted). Covers the OWASP "HTML body" + "HTML attribute" contexts.
pub fn h(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Make a JSON string safe to inline inside a `<script>` block by re-encoding
/// the few characters that could close the script tag or break JS parsing.
/// Each replacement is still valid JSON, so the runtime value is unchanged.
fn json_for_script(s: &str) -> String {
    s.replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

pub const CSS: &str = r#"
* { box-sizing: border-box; margin: 0; padding: 0; }
body {
    background: #0c1222;
    color: #f1f5f9;
    font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif;
    line-height: 1.6;
    padding: 1em;
    max-width: 900px;
    margin: 0 auto;
}
h1, h2, h3 { margin: 0.5em 0; color: #22d3ee; }
a { color: #22d3ee; }
input, select, button, textarea {
    background: #0f172a;
    color: #f1f5f9;
    border: 1px solid #1e293b;
    padding: 0.5em 0.75em;
    border-radius: 4px;
    font-size: 1em;
    font-family: inherit;
}
input:focus, select:focus, textarea:focus {
    outline: none;
    border-color: #22d3ee;
}
button {
    background: #1e293b;
    cursor: pointer;
    transition: background 0.2s;
}
button:hover { background: #334155; }
button:disabled { opacity: 0.5; cursor: not-allowed; }
button.primary { background: #16a34a; }
button.primary:hover { background: #22c55e; }
button.danger { background: #dc2626; }
button.danger:hover { background: #ef4444; }
.form-group {
    margin-bottom: 1em;
}
.form-group label {
    display: block;
    margin-bottom: 0.25em;
    color: #22d3ee;
    font-size: 0.9em;
}
.form-group input, .form-group select {
    width: 100%;
}
.form-group input[type="checkbox"] {
    width: auto;
    margin: 0.25em 0 0;
}
.modal-backdrop {
    position: fixed;
    inset: 0;
    background: rgba(12, 18, 34, 0.75);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 1000;
    padding: 1em;
}
.modal {
    background: #0f172a;
    border: 1px solid #1e293b;
    border-radius: 8px;
    padding: 1.25em 1.5em;
    max-width: 640px;
    width: 100%;
    max-height: 90vh;
    overflow-y: auto;
    box-shadow: 0 10px 30px rgba(0, 0, 0, 0.5);
}
.consent-disclaimer {
    background: #0c1222;
    border: 1px solid #1e293b;
    border-radius: 4px;
    padding: 0.75em 1em;
    color: #f1f5f9;
    /* Prose, not code: inherit the page body font instead of forcing a
       monospace stack whose named fonts (SF Mono, Consolas) don't exist on
       Linux and fall back to the ugly default `monospace` glyphs. */
    font-family: inherit;
    font-size: 0.95em;
    line-height: 1.5;
    white-space: pre-wrap;
    word-break: break-word;
    margin: 0.75em 0;
}
.status-badge {
    display: inline-block;
    padding: 0.25em 0.75em;
    border-radius: 12px;
    font-size: 0.85em;
    font-weight: bold;
}
.status-disconnected { background: #450a0a; color: #ef4444; }
.status-agwpe-connected { background: #052e16; color: #22c55e; }
.status-connecting { background: #422006; color: #fbbf24; }
.status-reconnecting { background: #422006; color: #fbbf24; }
.status-connected { background: #052e16; color: #22c55e; }
.status-error { background: #450a0a; color: #ef4444; }
.btn-row {
    display: flex;
    gap: 0.5em;
    margin-top: 1em;
}
.card {
    background: #0f172a;
    border: 1px solid #1e293b;
    border-radius: 8px;
    padding: 1em;
    margin-bottom: 1em;
}
.debug-log {
    /* System monospace fallback chain: try the OS's own before dropping to
       the CSS generic, which on Linux without SF Mono/Fira Code/Consolas
       renders as an unstyled bitmap. */
    font-family: ui-monospace, 'Cascadia Mono', 'JetBrains Mono', 'DejaVu Sans Mono', 'Liberation Mono', Menlo, Consolas, monospace;
    font-size: 0.8em;
    background: #020617;
    border: 1px solid #1e293b;
    border-radius: 4px;
    padding: 0.5em;
    height: 300px;
    overflow-y: auto;
    white-space: pre-wrap;
    word-break: break-all;
}
.debug-log .log-entry { margin-bottom: 2px; }
.debug-log .log-info { color: #22d3ee; }
.debug-log .log-debug { color: #64748b; }
.debug-log .log-trace { color: #475569; }
.debug-log .log-tx { color: #fbbf24; }
.debug-log .log-rx { color: #22c55e; }
.debug-log .log-error { color: #ef4444; }
.debug-log .log-state { color: #a78bfa; }
.log-controls {
    display: flex;
    gap: 0.5em;
    margin-bottom: 0.5em;
    align-items: center;
}
.log-controls label { font-size: 0.85em; color: #22d3ee; }
.log-controls select { padding: 0.25em 0.5em; font-size: 0.85em; }
nav {
    display: flex;
    gap: 1em;
    margin-bottom: 1em;
    padding-bottom: 0.5em;
    border-bottom: 1px solid #1e293b;
}
nav a {
    text-decoration: none;
    padding: 0.25em 0.5em;
    border-radius: 4px;
}
nav a:hover { background: #1e293b; }
nav a.active { background: #1e293b; color: #f1f5f9; }
.msg { padding: 0.5em 0.75em; border-radius: 4px; margin-bottom: 1em; }
.msg-success { background: #052e16; color: #22c55e; border: 1px solid #22c55e; }
.msg-error { background: #450a0a; color: #ef4444; border: 1px solid #ef4444; }
.browse-bar {
    display: flex;
    gap: 0.5em;
    margin-bottom: 1em;
}
.browse-bar input { flex: 1; }
"#;

pub fn connect_page(
    my_callsign: &str,
    target_callsign: &str,
    connection_state: &str,
    connection_state_class: &str,
    ports_json: &str,
) -> String {
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <meta http-equiv="Content-Security-Policy" content="default-src 'self'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'">
    <title>Packet Browser - Connect</title>
    <style>{css}</style>
</head>
<body>
    <nav>
        <a href="/connect" class="active">Connect</a>
        <a href="/browse">Browse</a>
        <a href="/configuration">Configuration</a>
    </nav>

    <h1>Packet Browser</h1>

    <div class="card">
        <h2>Connection</h2>
        <p>Status: <span id="status-badge" class="status-badge {state_class}">{state}</span></p>

        <div class="form-group">
            <label for="my-call">My Callsign</label>
            <input type="text" id="my-call" value="{my_call}" placeholder="N0CALL" autocomplete="off">
        </div>

        <div class="form-group">
            <label for="target-call">Target Callsign</label>
            <input type="text" id="target-call" value="{target_call}" placeholder="NODE1" autocomplete="off">
        </div>

        <div class="form-group">
            <label for="port-select">AGWPE Port</label>
            <select id="port-select">
                <option value="">-- query AGWPE for ports --</option>
            </select>
        </div>

        <div class="btn-row">
            <button id="btn-agwpe" onclick="connectAgwpe()">Connect to AGWPE</button>
            <button id="btn-connect" class="primary" onclick="ax25Connect()" disabled>AX.25 Connect</button>
            <button id="btn-disconnect" class="danger" onclick="ax25Disconnect()" disabled>Disconnect</button>
        </div>
    </div>

    <div id="msg-area"></div>

    <div id="consent-modal" class="modal-backdrop" style="display:none" role="dialog" aria-modal="true" aria-labelledby="consent-modal-title">
        <div class="modal">
            <h2 id="consent-modal-title">Confirm connection</h2>
            <p>The remote station is asking you to acknowledge the following notice before continuing:</p>
            <pre id="consent-disclaimer" class="consent-disclaimer"></pre>
            <p>Only agree if you accept the notice above.</p>
            <div class="btn-row">
                <button class="primary" onclick="submitConsent(true)">I Agree</button>
                <button class="danger" onclick="submitConsent(false)">Decline &amp; Disconnect</button>
            </div>
        </div>
    </div>

    <div class="card">
        <h2>Debug Log</h2>
        <div class="log-controls">
            <label for="log-filter">Level:</label>
            <select id="log-filter" onchange="filterLogs()">
                <option value="all">All</option>
                <option value="info">Info</option>
                <option value="debug">Debug</option>
                <option value="trace">Trace</option>
            </select>
            <button onclick="clearLogs()">Clear</button>
        </div>
        <div id="debug-log" class="debug-log"></div>
    </div>

    <script>
        let ports = {ports_json};
        let logEntries = [];
        let eventSource = null;

        function initPorts() {{
            const sel = document.getElementById('port-select');
            sel.innerHTML = '';
            if (ports.length === 0) {{
                sel.innerHTML = '<option value="">-- no ports found --</option>';
                return;
            }}
            ports.forEach(p => {{
                const opt = document.createElement('option');
                opt.value = p.port_num;
                opt.textContent = p.port_num + ': ' + p.description;
                sel.appendChild(opt);
            }});
        }}

        function updateUI(state) {{
            const badge = document.getElementById('status-badge');
            badge.textContent = state;
            badge.className = 'status-badge status-' + state.toLowerCase().replace(/[^a-z]/g, '-');

            const btnAgwpe = document.getElementById('btn-agwpe');
            const btnConnect = document.getElementById('btn-connect');
            const btnDisconnect = document.getElementById('btn-disconnect');

            const busy = (state === 'Connecting' || state === 'Awaiting consent');
            btnAgwpe.disabled = (state === 'AGWPE Connected' || busy || state === 'Connected');
            btnConnect.disabled = (state !== 'AGWPE Connected');
            btnDisconnect.disabled = (state !== 'Connected' && !busy);

            if (state === 'Awaiting consent') {{
                openConsentModal();
            }} else {{
                closeConsentModal();
            }}
        }}

        let consentOpen = false;
        async function openConsentModal() {{
            if (consentOpen) return;
            consentOpen = true;
            try {{
                const resp = await fetch('/api/consent');
                const data = await resp.json();
                if (!data.awaiting) {{ consentOpen = false; return; }}
                document.getElementById('consent-disclaimer').textContent =
                    data.disclaimer || '(no disclaimer text provided)';
                document.getElementById('consent-modal').style.display = 'flex';
            }} catch (e) {{
                consentOpen = false;
                showMsg('Could not fetch consent prompt: ' + e.message, true);
            }}
        }}

        function closeConsentModal() {{
            document.getElementById('consent-modal').style.display = 'none';
            consentOpen = false;
        }}

        async function submitConsent(accepted) {{
            closeConsentModal();
            try {{
                const resp = await fetch('/api/consent', {{
                    method: 'POST',
                    headers: {{ 'Content-Type': 'application/json' }},
                    body: JSON.stringify({{ accepted: accepted }})
                }});
                const data = await resp.json();
                if (!data.ok) {{
                    showMsg(data.error || 'Consent submission failed', true);
                    return;
                }}
                if (!accepted) {{
                    showMsg('Declined. Disconnecting.');
                    // Give the background handshake a moment to unwind, then
                    // force-tear-down so we don't leave a half-open session.
                    setTimeout(() => {{ ax25Disconnect(); }}, 250);
                }}
            }} catch (e) {{
                showMsg('Error: ' + e.message, true);
            }}
        }}

        function showMsg(text, isError) {{
            const area = document.getElementById('msg-area');
            area.innerHTML = '<div class="msg ' + (isError ? 'msg-error' : 'msg-success') + '">' + text + '</div>';
            setTimeout(() => area.innerHTML = '', 5000);
        }}

        async function connectAgwpe() {{
            const btn = document.getElementById('btn-agwpe');
            btn.disabled = true;
            btn.textContent = 'Connecting...';
            try {{
                const resp = await fetch('/api/agwpe-status', {{ method: 'POST' }});
                const data = await resp.json();
                if (data.ok) {{
                    ports = data.ports || [];
                    initPorts();
                    updateUI(data.state);
                    showMsg('Connected to AGWPE');
                }} else {{
                    updateUI(data.state || 'Error');
                    showMsg(data.error || 'Failed to connect to AGWPE', true);
                }}
            }} catch (e) {{
                showMsg('Error: ' + e.message, true);
                updateUI('Error');
            }}
            btn.textContent = 'Connect to AGWPE';
        }}

        async function ax25Connect() {{
            const target = document.getElementById('target-call').value.trim();
            const portNum = document.getElementById('port-select').value;
            if (!target) {{ showMsg('Enter a target callsign', true); return; }}
            if (portNum === '') {{ showMsg('Select an AGWPE port first', true); return; }}

            const btn = document.getElementById('btn-connect');
            btn.disabled = true;
            btn.textContent = 'Connecting...';
            updateUI('Connecting');
            try {{
                const resp = await fetch('/api/connect', {{
                    method: 'POST',
                    headers: {{ 'Content-Type': 'application/json' }},
                    body: JSON.stringify({{ target_callsign: target, port_num: parseInt(portNum) }})
                }});
                const data = await resp.json();
                if (data.ok) {{
                    updateUI('Connected');
                    showMsg('AX.25 connected to ' + target + '. Opening browser…');
                    // Send the user straight to the browse UI so there's an
                    // obvious next step; the connect page has no other job
                    // once the link is up.
                    window.location.href = '/browse';
                    return;
                }} else {{
                    updateUI(data.state || 'Error');
                    showMsg(data.error || 'Connection failed', true);
                }}
            }} catch (e) {{
                showMsg('Error: ' + e.message, true);
                updateUI('Error');
            }}
            btn.textContent = 'AX.25 Connect';
            btn.disabled = false;
        }}

        async function ax25Disconnect() {{
            try {{
                const resp = await fetch('/api/disconnect', {{ method: 'POST' }});
                const data = await resp.json();
                updateUI('Disconnected');
                showMsg('Disconnected');
            }} catch (e) {{
                showMsg('Error: ' + e.message, true);
            }}
        }}

        function addLogEntry(entry) {{
            logEntries.push(entry);
            if (logEntries.length > 1000) logEntries.shift();
            renderLogs();
        }}

        function renderLogs() {{
            const log = document.getElementById('debug-log');
            const filter = document.getElementById('log-filter').value;
            let html = '';
            for (const e of logEntries) {{
                if (filter !== 'all' && e.level.toLowerCase() !== filter) continue;
                const dir = e.direction ? ('[' + e.direction + '] ') : '';
                const cls = 'log-entry log-' + e.level.toLowerCase()
                    + (e.direction ? ' log-' + e.direction.toLowerCase() : '')
                    + (e.category === 'STATE' ? ' log-state' : '')
                    + (e.category === 'ERROR' ? ' log-error' : '');
                const ts = e.timestamp ? e.timestamp.substring(11, 23) : '';
                html += '<div class="' + cls + '">' + ts + ' ' + dir + e.category + ': ' + escapeHtml(e.message) + '</div>';
            }}
            log.innerHTML = html;
            log.scrollTop = log.scrollHeight;
        }}

        function filterLogs() {{ renderLogs(); }}

        function clearLogs() {{
            logEntries = [];
            renderLogs();
        }}

        function escapeHtml(s) {{
            const d = document.createElement('div');
            d.textContent = s;
            return d.innerHTML;
        }}

        function connectSSE() {{
            if (eventSource) eventSource.close();
            eventSource = new EventSource('/events');
            eventSource.onmessage = function(event) {{
                try {{
                    const entry = JSON.parse(event.data);
                    addLogEntry(entry);
                    // State transitions arrive as STATE-category log lines of
                    // the form "State changed to: <name>". Parse them so the
                    // UI can react to the AwaitingConsent → Connected flip
                    // even while /api/connect is still awaiting server-side.
                    if (entry.category === 'STATE') {{
                        const m = /^State changed to:\s*(.+)$/.exec(entry.message);
                        if (m) updateUI(m[1].trim());
                    }}
                }} catch (e) {{}}
            }};
            eventSource.onerror = function() {{
                setTimeout(connectSSE, 3000);
            }};
        }}

        initPorts();
        updateUI('{state}');
        connectSSE();

        fetch('/api/agwpe-status').then(r => r.json()).then(data => {{
            if (data.ports) {{
                ports = data.ports;
                initPorts();
            }}
            if (data.state) updateUI(data.state);
        }}).catch(() => {{}});
    </script>
</body>
</html>"#,
        css = CSS,
        state = h(connection_state),
        state_class = h(connection_state_class),
        my_call = h(my_callsign),
        target_call = h(target_callsign),
        ports_json = json_for_script(ports_json),
    )
}

pub fn configuration_page(
    agwpe_host: &str,
    agwpe_port: u16,
    my_callsign: &str,
    target_callsign: &str,
    bpq_command: &str,
    skip_bpq_app: bool,
) -> String {
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <meta http-equiv="Content-Security-Policy" content="default-src 'self'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'">
    <title>Packet Browser - Configuration</title>
    <style>{css}</style>
</head>
<body>
    <nav>
        <a href="/connect">Connect</a>
        <a href="/browse">Browse</a>
        <a href="/configuration" class="active">Configuration</a>
    </nav>

    <h1>Configuration</h1>

    <div id="msg-area"></div>

    <div class="card">
        <h2>AGWPE Settings</h2>

        <div class="form-group">
            <label for="agwpe-host">AGWPE Host</label>
            <input type="text" id="agwpe-host" value="{host}" placeholder="127.0.0.1">
        </div>

        <div class="form-group">
            <label for="agwpe-port">AGWPE Port</label>
            <input type="number" id="agwpe-port" value="{port}" placeholder="8000">
        </div>
    </div>

    <div class="card">
        <h2>Session Settings</h2>

        <div class="form-group">
            <label for="my-callsign">My Callsign</label>
            <input type="text" id="my-callsign" value="{my_callsign}" placeholder="N0CALL">
            <small>Your amateur radio callsign</small>
        </div>

        <div class="form-group">
            <label for="target-callsign">Target Callsign</label>
            <input type="text" id="target-callsign" value="{target_callsign}" placeholder="NODE1">
            <small>The BPQ node or station to connect to</small>
        </div>

        <div class="form-group">
            <label for="skip-bpq-app">Skip BPQ Application Command</label>
            <input type="checkbox" id="skip-bpq-app" {skip_checked} onchange="updateBpqCommandVisibility()">
            <small>Enable if connecting directly to a node SSID that doesn't require an application command</small>
        </div>

        <div class="form-group" id="bpq-command-group">
            <label for="bpq-command">BPQ Application Command</label>
            <input type="text" id="bpq-command" value="{bpq_command}" placeholder="WEB">
            <small>The application command sent after connecting (e.g., WEB, BBS)</small>
        </div>

        <div class="btn-row">
            <button class="primary" onclick="saveConfig()">Save Configuration</button>
            <button onclick="testConnection()">Test AGWPE Connection</button>
        </div>
    </div>

    <script>
        function showMsg(text, isError) {{
            const area = document.getElementById('msg-area');
            area.innerHTML = '<div class="msg ' + (isError ? 'msg-error' : 'msg-success') + '">' + text + '</div>';
            setTimeout(() => area.innerHTML = '', 5000);
        }}

        function updateBpqCommandVisibility() {{
            const skipped = document.getElementById('skip-bpq-app').checked;
            document.getElementById('bpq-command-group').style.display = skipped ? 'none' : '';
        }}

        async function loadConfig() {{
            try {{
                const resp = await fetch('/api/config');
                const data = await resp.json();
                document.getElementById('agwpe-host').value = data.agwpe_host || '127.0.0.1';
                document.getElementById('agwpe-port').value = data.agwpe_port || 8000;
                document.getElementById('my-callsign').value = data.my_callsign || '';
                document.getElementById('target-callsign').value = data.target_callsign || '';
                document.getElementById('bpq-command').value = data.bpq_command || 'WEB';
                document.getElementById('skip-bpq-app').checked = data.skip_bpq_app || false;
                updateBpqCommandVisibility();
            }} catch (e) {{
                showMsg('Failed to load config: ' + e.message, true);
            }}
        }}

        async function saveConfig() {{
            const host = document.getElementById('agwpe-host').value.trim();
            const port = parseInt(document.getElementById('agwpe-port').value);
            const myCallsign = document.getElementById('my-callsign').value.trim();
            const targetCallsign = document.getElementById('target-callsign').value.trim();
            const bpqCommand = document.getElementById('bpq-command').value.trim();
            const skipBpqApp = document.getElementById('skip-bpq-app').checked;

            if (!host) {{ showMsg('AGWPE Host is required', true); return; }}
            if (!port || port < 1 || port > 65535) {{ showMsg('Invalid AGWPE port', true); return; }}
            if (!myCallsign) {{ showMsg('My Callsign is required', true); return; }}

            try {{
                const resp = await fetch('/api/config', {{
                    method: 'POST',
                    headers: {{ 'Content-Type': 'application/json' }},
                    body: JSON.stringify({{
                        agwpe_host: host,
                        agwpe_port: port,
                        my_callsign: myCallsign,
                        target_callsign: targetCallsign,
                        bpq_command: bpqCommand,
                        skip_bpq_app: skipBpqApp
                    }})
                }});
                const data = await resp.json();
                if (data.ok) {{
                    showMsg('Configuration saved successfully');
                }} else {{
                    showMsg(data.error || 'Failed to save', true);
                }}
            }} catch (e) {{
                showMsg('Error: ' + e.message, true);
            }}
        }}

        async function testConnection() {{
            try {{
                const resp = await fetch('/api/agwpe-status', {{ method: 'POST' }});
                const data = await resp.json();
                if (data.ok) {{
                    showMsg('AGWPE reachable. ' + (data.ports || []).length + ' port(s) found.');
                }} else {{
                    showMsg(data.error || 'AGWPE unreachable', true);
                }}
            }} catch (e) {{
                showMsg('Error: ' + e.message, true);
            }}
        }}

        loadConfig();
    </script>
</body>
</html>"#,
        css = CSS,
        host = h(agwpe_host),
        port = agwpe_port,
        my_callsign = h(my_callsign),
        target_callsign = h(target_callsign),
        bpq_command = h(bpq_command),
        skip_checked = if skip_bpq_app { "checked" } else { "" },
    )
}

pub fn error_page(message: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <title>Packet Browser - Error</title>
    <style>{css}</style>
</head>
<body>
    <nav>
        <a href="/connect">Connect</a>
        <a href="/browse">Browse</a>
        <a href="/configuration">Configuration</a>
    </nav>

    <h1>Error</h1>
    <div class="card">
        <p class="msg msg-error">{message}</p>
        <p><a href="/connect">Return to Connect page</a></p>
    </div>
</body>
</html>"#,
        css = CSS,
        message = h(message),
    )
}

pub fn render_session_error_page(message: &str, show_reconnect_link: bool) -> String {
    let reconnect_link = if show_reconnect_link {
        r#"<p><a href="/connect">Reconnect</a></p>"#
    } else {
        ""
    };
    format!(
        r#"<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8"><title>Session error</title><style>{css}</style></head>
<body style="font-family: sans-serif; max-width: 600px; margin: 4em auto; padding: 1em;">
<h1>Session error</h1>
<p>{message}</p>
{reconnect_link}
</body></html>"#,
        css = CSS,
        message = h(message),
        reconnect_link = reconnect_link,
    )
}

pub fn browse_page(html_content: &str, url: &str) -> String {
    let escaped_url = h(url);

    // Style rules here are scoped to `.browse-header` so the client's chrome
    // stays consistent while the fetched content below renders under browser
    // defaults + whatever inline CSS the author's <style>/style="" blocks
    // supplied. Global body/a/h1/input rules from the shared CSS const are
    // deliberately not included — they'd cascade into browse-content and
    // repaint every page in the client's palette, which is exactly what the
    // reader was complaining about.
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; img-src data:; form-action 'self'; base-uri 'none'; frame-ancestors 'none'">
    <title>Packet Browser</title>
    <style>
    .browse-header, .browse-header * {{
        box-sizing: border-box;
        margin: 0;
        padding: 0;
        font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif;
    }}
    .browse-header {{
        position: fixed;
        top: 0;
        left: 0;
        right: 0;
        z-index: 2147483647;
        background: #0f172a;
        color: #f1f5f9;
        border-bottom: 1px solid #1e293b;
        padding: 0.5em 1em;
        display: flex;
        gap: 0.5em;
        align-items: center;
    }}
    .browse-header a {{
        color: #22d3ee;
        font-size: 0.85em;
        white-space: nowrap;
        text-decoration: none;
    }}
    .browse-header a:hover {{ text-decoration: underline; }}
    .browse-header input {{
        background: #0c1222;
        color: #f1f5f9;
        border: 1px solid #1e293b;
        border-radius: 4px;
        padding: 0.4em 0.6em;
        font-size: 0.9em;
        flex: 1;
    }}
    .browse-header input:focus {{ outline: none; border-color: #22d3ee; }}
    .browse-header button {{
        background: #16a34a;
        color: #f1f5f9;
        border: 1px solid #1e293b;
        border-radius: 4px;
        padding: 0.4em 0.9em;
        font-size: 0.9em;
        cursor: pointer;
    }}
    .browse-header button:hover {{ background: #22c55e; }}
    /* Leave room for the fixed header so fetched content isn't hidden under
       it. margin-top lives on browse-content (not body) so we don't fight
       with body-level rules from the fetched CSS. */
    .browse-content {{ margin-top: 3.25em; }}
    </style>
</head>
<body>
    <div class="browse-header">
        <a href="/connect">Connect</a>
        <a href="/configuration">Config</a>
        <a href="/cache">Cache</a>
        <a href="/browse?url={url}&amp;nocache=1" title="Bypass cache and refetch">Reload</a>
        <form action="/browse" method="GET" style="display:flex;gap:0.5em;flex:1;margin:0">
            <input type="text" name="url" value="{url}" placeholder="Enter a URL, e.g. https://example.com" autocomplete="off" autofocus>
            <button type="submit">Go</button>
        </form>
    </div>
    <div class="browse-content">
        {content}
    </div>
</body>
</html>"#,
        url = escaped_url,
        content = html_content,
    )
}

pub struct CachePageRow {
    pub url: String,
    pub size_bytes: u64,
    pub fetched_at_iso: String,
    pub last_used_iso: String,
    pub ttl_remaining_secs: i64,
    pub etag: String,
}

pub fn cache_page(rows: &[CachePageRow], total_bytes: u64, cap_bytes: u64) -> String {
    let mut body = String::new();
    body.push_str(&format!(
        "<p>{} entries — {} / {} bytes used.</p>",
        rows.len(),
        total_bytes,
        cap_bytes,
    ));
    body.push_str(r#"<form method="POST" action="/api/cache/clear" style="margin-bottom:1em"><button class="danger" type="submit">Clear all</button></form>"#);
    body.push_str(r#"<table style="width:100%;border-collapse:collapse">"#);
    body.push_str(r#"<thead><tr><th style="text-align:left">URL</th><th>Size</th><th>Fetched</th><th>Last used</th><th>TTL left</th><th></th></tr></thead><tbody>"#);
    for row in rows {
        body.push_str(&format!(
            r#"<tr>
                <td style="max-width:40em;overflow:hidden;text-overflow:ellipsis">{url}</td>
                <td>{size}</td>
                <td>{fetched}</td>
                <td>{used}</td>
                <td>{ttl}s</td>
                <td>
                    <form method="POST" action="/api/cache/delete" style="margin:0">
                        <input type="hidden" name="url" value="{url_attr}">
                        <button class="danger" type="submit">Delete</button>
                    </form>
                </td>
            </tr>"#,
            url = h(&row.url),
            size = row.size_bytes,
            fetched = h(&row.fetched_at_iso),
            used = h(&row.last_used_iso),
            ttl = row.ttl_remaining_secs,
            url_attr = h(&row.url),
        ));
    }
    body.push_str("</tbody></table>");

    format!(
        r#"<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8"><title>Cache</title><style>{css}</style></head>
<body><h1>Cache</h1><p><a href="/browse">Back to browse</a></p>{body}</body></html>"#,
        css = CSS,
        body = body,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_error_page_shows_message_and_link() {
        let html = render_session_error_page("test message", true);
        assert!(html.contains("test message"));
        assert!(html.contains("href=\"/connect\""));

        let html_no_link = render_session_error_page("no link message", false);
        assert!(html_no_link.contains("no link message"));
        assert!(!html_no_link.contains("href=\"/connect\""));
    }

    #[test]
    fn test_session_error_page_escapes_html() {
        let html = render_session_error_page("bad <script>alert(1)</script> input", false);
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;"));
    }
}
