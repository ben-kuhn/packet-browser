pub const CSS: &str = r#"
* { box-sizing: border-box; margin: 0; padding: 0; }
body {
    background: #1a1a2e;
    color: #e0e0e0;
    font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif;
    line-height: 1.6;
    padding: 1em;
    max-width: 900px;
    margin: 0 auto;
}
h1, h2, h3 { margin: 0.5em 0; color: #a0c4ff; }
a { color: #4da6ff; }
input, select, button, textarea {
    background: #16213e;
    color: #e0e0e0;
    border: 1px solid #0f3460;
    padding: 0.5em 0.75em;
    border-radius: 4px;
    font-size: 1em;
    font-family: inherit;
}
input:focus, select:focus, textarea:focus {
    outline: none;
    border-color: #4da6ff;
}
button {
    background: #0f3460;
    cursor: pointer;
    transition: background 0.2s;
}
button:hover { background: #1a4a80; }
button:disabled { opacity: 0.5; cursor: not-allowed; }
button.primary { background: #1a6b3c; }
button.primary:hover { background: #228b4e; }
button.danger { background: #8b1a1a; }
button.danger:hover { background: #a52a2a; }
.form-group {
    margin-bottom: 1em;
}
.form-group label {
    display: block;
    margin-bottom: 0.25em;
    color: #a0c4ff;
    font-size: 0.9em;
}
.form-group input, .form-group select {
    width: 100%;
}
.status-badge {
    display: inline-block;
    padding: 0.25em 0.75em;
    border-radius: 12px;
    font-size: 0.85em;
    font-weight: bold;
}
.status-disconnected { background: #3d1515; color: #f44336; }
.status-agwpe-connected { background: #153d2e; color: #4caf50; }
.status-connecting { background: #3d2e15; color: #ff9800; }
.status-connected { background: #153d1a; color: #4caf50; }
.status-error { background: #3d1515; color: #f44336; }
.btn-row {
    display: flex;
    gap: 0.5em;
    margin-top: 1em;
}
.card {
    background: #16213e;
    border: 1px solid #0f3460;
    border-radius: 8px;
    padding: 1em;
    margin-bottom: 1em;
}
.debug-log {
    font-family: 'SF Mono', 'Fira Code', 'Consolas', monospace;
    font-size: 0.8em;
    background: #0a0a1a;
    border: 1px solid #0f3460;
    border-radius: 4px;
    padding: 0.5em;
    height: 300px;
    overflow-y: auto;
    white-space: pre-wrap;
    word-break: break-all;
}
.debug-log .log-entry { margin-bottom: 2px; }
.debug-log .log-info { color: #a0c4ff; }
.debug-log .log-debug { color: #808080; }
.debug-log .log-trace { color: #606060; }
.debug-log .log-tx { color: #ff9800; }
.debug-log .log-rx { color: #4caf50; }
.debug-log .log-error { color: #f44336; }
.debug-log .log-state { color: #ce93d8; }
.log-controls {
    display: flex;
    gap: 0.5em;
    margin-bottom: 0.5em;
    align-items: center;
}
.log-controls label { font-size: 0.85em; color: #a0c4ff; }
.log-controls select { padding: 0.25em 0.5em; font-size: 0.85em; }
nav {
    display: flex;
    gap: 1em;
    margin-bottom: 1em;
    padding-bottom: 0.5em;
    border-bottom: 1px solid #0f3460;
}
nav a {
    text-decoration: none;
    padding: 0.25em 0.5em;
    border-radius: 4px;
}
nav a:hover { background: #0f3460; }
nav a.active { background: #0f3460; color: #fff; }
.msg { padding: 0.5em 0.75em; border-radius: 4px; margin-bottom: 1em; }
.msg-success { background: #153d1a; color: #4caf50; border: 1px solid #228b4e; }
.msg-error { background: #3d1515; color: #f44336; border: 1px solid #a52a2a; }
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
    <title>Packet Browser - Connect</title>
    <style>{css}</style>
</head>
<body>
    <nav>
        <a href="/connect" class="active">Connect</a>
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

            btnAgwpe.disabled = (state === 'AGWPE Connected' || state === 'Connecting' || state === 'Connected');
            btnConnect.disabled = (state !== 'AGWPE Connected');
            btnDisconnect.disabled = (state !== 'Connected' && state !== 'Connecting');
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
                    showMsg('AX.25 connected to ' + target);
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
        state = connection_state,
        state_class = connection_state_class,
        my_call = my_callsign,
        target_call = target_callsign,
        ports_json = ports_json,
    )
}

pub fn configuration_page(agwpe_host: &str, agwpe_port: u16) -> String {
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <title>Packet Browser - Configuration</title>
    <style>{css}</style>
</head>
<body>
    <nav>
        <a href="/connect">Connect</a>
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

        <div class="btn-row">
            <button class="primary" onclick="saveConfig()">Save</button>
            <button onclick="testConnection()">Test Connection</button>
        </div>
    </div>

    <script>
        function showMsg(text, isError) {{
            const area = document.getElementById('msg-area');
            area.innerHTML = '<div class="msg ' + (isError ? 'msg-error' : 'msg-success') + '">' + text + '</div>';
            setTimeout(() => area.innerHTML = '', 5000);
        }}

        async function loadConfig() {{
            try {{
                const resp = await fetch('/api/config');
                const data = await resp.json();
                document.getElementById('agwpe-host').value = data.agwpe_host || '127.0.0.1';
                document.getElementById('agwpe-port').value = data.agwpe_port || 8000;
            }} catch (e) {{
                showMsg('Failed to load config: ' + e.message, true);
            }}
        }}

        async function saveConfig() {{
            const host = document.getElementById('agwpe-host').value.trim();
            const port = parseInt(document.getElementById('agwpe-port').value);
            if (!host) {{ showMsg('Host is required', true); return; }}
            if (!port || port < 1 || port > 65535) {{ showMsg('Invalid port', true); return; }}

            try {{
                const resp = await fetch('/api/config', {{
                    method: 'POST',
                    headers: {{ 'Content-Type': 'application/json' }},
                    body: JSON.stringify({{ agwpe_host: host, agwpe_port: port }})
                }});
                const data = await resp.json();
                if (data.ok) {{
                    showMsg('Configuration saved');
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
        host = agwpe_host,
        port = agwpe_port,
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
        message = message,
    )
}

pub fn browse_page(html_content: &str, url: &str) -> String {
    let escaped_url = url
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;");
    
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <title>Packet Browser</title>
    <style>{css}
    .browse-header {{
        background: #16213e;
        border-bottom: 1px solid #0f3460;
        padding: 0.5em 1em;
        display: flex;
        gap: 0.5em;
        align-items: center;
    }}
    .browse-header input {{
        flex: 1;
        font-size: 0.9em;
    }}
    .browse-header a {{
        font-size: 0.85em;
        white-space: nowrap;
    }}
    .browse-content {{
        padding: 1em;
    }}
    </style>
</head>
<body style="max-width:none;padding:0">
    <div class="browse-header">
        <a href="/connect">Back</a>
        <form action="/browse" method="GET" style="display:flex;gap:0.5em;flex:1">
            <input type="text" name="url" value="{url}" placeholder="Enter URL...">
            <button type="submit">Go</button>
        </form>
    </div>
    <div class="browse-content">
        {content}
    </div>
</body>
</html>"#,
        css = CSS,
        url = escaped_url,
        content = html_content,
    )
}
