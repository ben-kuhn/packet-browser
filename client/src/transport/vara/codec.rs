use super::super::{VaraBandwidth, VaraMode};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VaraResponse {
    Ok,
    Pending,
    Connected { local: String, remote: String },
    Disconnected,
    BusyDetected,
    LinkRegistered,
    Buffer(u32),
    Missing(String),
    Unknown(String),
}

pub fn parse_line(raw: &str) -> VaraResponse {
    let s = raw.trim_end_matches(|c: char| c == '\r' || c == '\n');
    if s == "OK" { return VaraResponse::Ok; }
    if s == "PENDING" { return VaraResponse::Pending; }
    if s == "DISCONNECTED" { return VaraResponse::Disconnected; }
    if s == "BUSY DETECTED" { return VaraResponse::BusyDetected; }
    if s == "LINK REGISTERED" { return VaraResponse::LinkRegistered; }
    if let Some(rest) = s.strip_prefix("CONNECTED ") {
        let mut it = rest.split_whitespace();
        if let (Some(local), Some(remote)) = (it.next(), it.next()) {
            return VaraResponse::Connected {
                local: local.to_string(),
                remote: remote.to_string(),
            };
        }
    }
    if let Some(rest) = s.strip_prefix("BUFFER ") {
        if let Ok(n) = rest.trim().parse::<u32>() {
            return VaraResponse::Buffer(n);
        }
    }
    if let Some(rest) = s.strip_prefix("MISSING ") {
        return VaraResponse::Missing(rest.trim().to_string());
    }
    VaraResponse::Unknown(s.to_string())
}

pub fn bandwidth_wire_command(bw: VaraBandwidth) -> &'static str {
    match bw {
        VaraBandwidth::VNarrow => "VNARROW",
        VaraBandwidth::VWide => "VWIDE",
        VaraBandwidth::Bw250 => "BW250",
        VaraBandwidth::Bw500 => "BW500",
        VaraBandwidth::Bw2300 => "BW2300",
        VaraBandwidth::Bw2750 => "BW2750",
    }
}

pub fn setup_commands(
    local_callsign: &str,
    _mode: VaraMode,
    bw: VaraBandwidth,
) -> Vec<String> {
    vec![
        format!("MYCALL {}", local_callsign),
        "LISTEN OFF".to_string(),
        "COMPRESSION OFF".to_string(),
        bandwidth_wire_command(bw).to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_known_responses() {
        assert!(matches!(parse_line("OK"), VaraResponse::Ok));
        assert!(matches!(parse_line("PENDING"), VaraResponse::Pending));
        assert!(matches!(parse_line("DISCONNECTED"), VaraResponse::Disconnected));
        assert!(matches!(parse_line("BUSY DETECTED"), VaraResponse::BusyDetected));
        assert!(matches!(parse_line("LINK REGISTERED"), VaraResponse::LinkRegistered));

        match parse_line("CONNECTED W1TEST N0CALL-8") {
            VaraResponse::Connected { local, remote } => {
                assert_eq!(local, "W1TEST");
                assert_eq!(remote, "N0CALL-8");
            }
            other => panic!("{other:?}"),
        }

        match parse_line("BUFFER 42") {
            VaraResponse::Buffer(n) => assert_eq!(n, 42),
            other => panic!("{other:?}"),
        }

        match parse_line("MISSING MYCALL") {
            VaraResponse::Missing(s) => assert_eq!(s, "MYCALL"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parse_line_trims_line_terminators() {
        assert!(matches!(parse_line("OK\r"), VaraResponse::Ok));
        assert!(matches!(parse_line("OK\r\n"), VaraResponse::Ok));
        assert!(matches!(parse_line("OK\n"), VaraResponse::Ok));
    }

    #[test]
    fn unknown_responses_are_preserved_for_debug_logging() {
        match parse_line("REGISTERED W1TEST 2026") {
            VaraResponse::Unknown(s) => assert_eq!(s, "REGISTERED W1TEST 2026"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn bandwidth_wire_command_matches_vara_spec() {
        assert_eq!(bandwidth_wire_command(VaraBandwidth::VNarrow), "VNARROW");
        assert_eq!(bandwidth_wire_command(VaraBandwidth::VWide), "VWIDE");
        assert_eq!(bandwidth_wire_command(VaraBandwidth::Bw250), "BW250");
        assert_eq!(bandwidth_wire_command(VaraBandwidth::Bw500), "BW500");
        assert_eq!(bandwidth_wire_command(VaraBandwidth::Bw2300), "BW2300");
        assert_eq!(bandwidth_wire_command(VaraBandwidth::Bw2750), "BW2750");
    }

    #[test]
    fn setup_commands_emit_expected_ordering() {
        let cmds = setup_commands("W1TEST", VaraMode::Fm, VaraBandwidth::VWide);
        assert_eq!(cmds, vec![
            "MYCALL W1TEST".to_string(),
            "LISTEN OFF".to_string(),
            "COMPRESSION OFF".to_string(),
            "VWIDE".to_string(),
        ]);
    }
}
