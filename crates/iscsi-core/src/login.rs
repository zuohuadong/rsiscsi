use crate::{
    ChapCredentials, Error, NegotiatedParams, SessionOptions, TargetAddr,
    config::validate_text,
    wire::{self, Bhs},
};
use socket2::{SockRef, TcpKeepalive};
use std::time::Duration;
use tokio::{io::AsyncWriteExt, net::TcpStream, time::timeout};

pub(crate) struct Connection {
    pub socket: TcpStream,
    pub params: NegotiatedParams,
    pub cmdsn: u32,
    pub expcmdsn: u32,
    pub maxcmdsn: u32,
    pub expstatsn: u32,
}

pub(crate) async fn connect(
    addr: &TargetAddr,
    iqn: Option<&str>,
    chap: Option<&ChapCredentials>,
    opts: &SessionOptions,
) -> Result<Connection, Error> {
    opts.validate()?;
    validate_text("host", &addr.host)?;
    if let Some(iqn) = iqn {
        validate_text("TargetName", iqn)?;
    }
    if let Some(chap) = chap {
        validate_text("CHAP_N", &chap.user)?;
    }
    timeout(opts.connect_timeout, async {
        let socket = TcpStream::connect((addr.host.as_str(), addr.port)).await?;
        socket.set_nodelay(opts.nodelay)?;
        let sock = SockRef::from(&socket);
        sock.set_recv_buffer_size(4 * 1024 * 1024)?;
        sock.set_send_buffer_size(4 * 1024 * 1024)?;
        sock.set_tcp_keepalive(&TcpKeepalive::new().with_time(Duration::from_secs(30)))?;
        let mut connection = Connection {
            socket,
            params: NegotiatedParams::initial(opts),
            cmdsn: 1,
            expcmdsn: 1,
            maxcmdsn: 1,
            expstatsn: 0,
        };
        negotiate(&mut connection, iqn, chap, opts).await?;
        Ok(connection)
    })
    .await
    .map_err(|_| Error::Timeout)?
}

async fn negotiate(
    conn: &mut Connection,
    iqn: Option<&str>,
    chap: Option<&ChapCredentials>,
    opts: &SessionOptions,
) -> Result<(), Error> {
    let mut isid: [u8; 6] = rand::random();
    isid[0] = 0x80;
    let mut tsih = [0; 2];
    let mut stage = 0u8;
    let mut chap_replied = false;
    let mut pairs = vec![
        ("InitiatorName", opts.initiator_name.clone()),
        (
            "SessionType",
            if iqn.is_some() { "Normal" } else { "Discovery" }.into(),
        ),
        (
            "AuthMethod",
            if chap.is_some() { "CHAP" } else { "None" }.into(),
        ),
    ];
    if let Some(iqn) = iqn {
        pairs.push(("TargetName", iqn.to_owned()));
    }
    let mut security_values = std::collections::HashMap::new();
    let mut fragments = Vec::new();
    let mut continuing = false;
    for _ in 0..32 {
        let flags = if continuing {
            stage << 2
        } else if stage == 0 {
            0x81
        } else {
            0x87
        };
        let mut h = Bhs::new(wire::LOGIN_REQUEST | 0x40, flags);
        h.0[8..14].copy_from_slice(&isid);
        h.0[14..16].copy_from_slice(&tsih);
        h.put_u32(16, 1);
        h.put_u32(24, conn.cmdsn);
        h.put_u32(28, conn.expstatsn);
        let mut encoded = Vec::new();
        wire::encode(&h, &[], &wire::text(&pairs), false, false, &mut encoded)?;
        conn.socket.write_all(&encoded).await?;
        let pdu = wire::read_pdu(&mut conn.socket, false, false, 65_536).await?;
        if pdu.header.opcode() != wire::LOGIN_RESPONSE || pdu.header.itt() != 1 {
            return Err(Error::Login("unexpected login response".into()));
        }
        let h = &pdu.header;
        if h.0[36] != 0 {
            return Err(Error::Login(format!(
                "target rejected login: class {}, detail {}",
                h.0[36], h.0[37]
            )));
        }
        if h.0[3] != 0 || (h.0[1] >> 2) & 3 != stage || h.0[8..14] != isid {
            return Err(Error::Login("invalid login version, stage or ISID".into()));
        }
        tsih.copy_from_slice(&h.0[14..16]);
        conn.expstatsn = h.u32(24).wrapping_add(1);
        conn.cmdsn = h.u32(28);
        conn.expcmdsn = h.u32(28);
        conn.maxcmdsn = h.u32(32);
        fragments.extend_from_slice(&pdu.data);
        if fragments.len() > 1_048_576 {
            return Err(Error::Login("login text exceeds limit".into()));
        }
        pairs.clear();
        continuing = h.0[1] & 0x40 != 0;
        if continuing {
            if h.0[1] & 0x80 != 0 {
                return Err(Error::Login("login C and T both set".into()));
            }
            continue;
        }
        let values = wire::parse_text(&fragments)?;
        fragments.clear();
        if stage == 0 {
            security_values.extend(values);
            if let Some(auth) = security_values.get("authmethod") {
                let expected = if chap.is_some() { "CHAP" } else { "None" };
                if auth != expected {
                    return Err(Error::Login(
                        "target selected an unoffered authentication method".into(),
                    ));
                }
            }
            if chap.is_some() && !chap_replied {
                if let (Some(algorithm), Some(identifier), Some(challenge)) = (
                    security_values.get("chap_a"),
                    security_values.get("chap_i"),
                    security_values.get("chap_c"),
                ) {
                    if algorithm != "5" {
                        return Err(Error::Login("only CHAP-MD5 is supported".into()));
                    }
                    let identifier = identifier
                        .parse::<u8>()
                        .map_err(|_| Error::Login("invalid CHAP_I".into()))?;
                    let challenge = wire::decode_binary(challenge)?;
                    if challenge.is_empty() {
                        return Err(Error::Login("empty CHAP challenge".into()));
                    }
                    if let Some(credentials) = chap {
                        pairs.push(("CHAP_N", credentials.user.clone()));
                        pairs.push((
                            "CHAP_R",
                            wire::chap_response(
                                identifier,
                                credentials.secret.as_bytes(),
                                &challenge,
                            ),
                        ));
                    }
                    chap_replied = true;
                } else {
                    pairs.push(("CHAP_A", "5".into()));
                }
            }
            if h.0[1] & 0x80 != 0 {
                if h.0[1] & 3 != 1 || (chap.is_some() && (!chap_replied || !pairs.is_empty())) {
                    return Err(Error::Login("premature security transition".into()));
                }
                stage = 1;
                pairs = operational_keys(opts, iqn.is_some());
            }
        } else {
            apply_params(&mut conn.params, &values)?;
            if h.0[1] & 0x80 != 0 {
                if h.0[1] & 3 != 3 {
                    return Err(Error::Login("invalid full-feature transition".into()));
                }
                if conn.params.header_digest != opts.header_digest
                    || conn.params.data_digest != opts.data_digest
                {
                    return Err(Error::Login(
                        "target did not accept requested digests".into(),
                    ));
                }
                if !conn.params.initial_r2t
                    || conn.params.error_recovery_level != 0
                    || conn.params.max_connections != 1
                    || conn.params.max_outstanding_r2t != 1
                {
                    return Err(Error::Login("unsupported operational parameters".into()));
                }
                if conn.params.first_burst_length > conn.params.max_burst_length {
                    return Err(Error::Login(
                        "FirstBurstLength exceeds MaxBurstLength".into(),
                    ));
                }
                return Ok(());
            }
            for (key, value) in &values {
                if value == "Reject" || value == "NotUnderstood" {
                    return Err(Error::Login(format!("target rejected {key}")));
                }
            }
        }
    }
    Err(Error::Login("login exchange exceeded limit".into()))
}

fn operational_keys(opts: &SessionOptions, normal: bool) -> Vec<(&'static str, String)> {
    let mut keys = vec![
        (
            "HeaderDigest",
            if opts.header_digest { "CRC32C" } else { "None" }.into(),
        ),
        (
            "DataDigest",
            if opts.data_digest { "CRC32C" } else { "None" }.into(),
        ),
        (
            "MaxRecvDataSegmentLength",
            opts.max_recv_data_segment_length.to_string(),
        ),
    ];
    if normal {
        keys.extend([
            ("InitialR2T", "Yes".into()),
            ("ImmediateData", "Yes".into()),
            ("MaxBurstLength", opts.max_burst_length.to_string()),
            ("FirstBurstLength", opts.first_burst_length.to_string()),
            ("DefaultTime2Wait", "2".into()),
            ("DefaultTime2Retain", "20".into()),
            ("ErrorRecoveryLevel", "0".into()),
            ("MaxConnections", "1".into()),
            ("MaxOutstandingR2T", "1".into()),
            ("DataPDUInOrder", "Yes".into()),
            ("DataSequenceInOrder", "Yes".into()),
        ]);
    }
    keys
}

fn apply_params(params: &mut NegotiatedParams, values: &[(String, String)]) -> Result<(), Error> {
    for (key, value) in values {
        let number = || {
            value
                .parse::<u32>()
                .map_err(|_| Error::Login(format!("invalid {key}")))
        };
        let boolean = || match value.as_str() {
            "Yes" => Ok(true),
            "No" => Ok(false),
            _ => Err(Error::Login(format!("invalid {key}"))),
        };
        match key.as_str() {
            "headerdigest" | "datadigest" => {
                let enabled = match value.as_str() {
                    "CRC32C" => true,
                    "None" => false,
                    _ => return Err(Error::Login(format!("unsupported {key}"))),
                };
                if key == "headerdigest" {
                    params.header_digest = enabled;
                } else {
                    params.data_digest = enabled;
                }
            }
            "maxrecvdatasegmentlength" => {
                let n = number()?;
                if !(512..=wire::MAX_DATA_LEN as u32).contains(&n) {
                    return Err(Error::Login("invalid target segment limit".into()));
                }
                params.target_max_recv_data_segment_length = n;
            }
            "maxburstlength" => params.max_burst_length = params.max_burst_length.min(number()?),
            "firstburstlength" => {
                params.first_burst_length = params.first_burst_length.min(number()?)
            }
            "initialr2t" => params.initial_r2t = boolean()?,
            "immediatedata" => params.immediate_data = boolean()?,
            "defaulttime2wait" => params.default_time2wait = number()?,
            "defaulttime2retain" => params.default_time2retain = number()?,
            "errorrecoverylevel" => params.error_recovery_level = number()?,
            "maxconnections" => params.max_connections = number()?,
            "maxoutstandingr2t" => params.max_outstanding_r2t = number()?,
            "datapduinorder" | "datasequenceinorder" if !boolean()? => {
                return Err(Error::Login("unordered data unsupported".into()));
            }
            _ => {}
        }
    }
    if params.max_burst_length < 512 || params.first_burst_length < 512 {
        return Err(Error::Login("invalid negotiated burst limits".into()));
    }
    Ok(())
}

pub(crate) fn serial_after(value: u32, previous: u32) -> bool {
    value != previous && value.wrapping_sub(previous) < 0x8000_0000
}

impl Connection {
    pub fn update_window(&mut self, expcmdsn: u32, maxcmdsn: u32) {
        update_window_edges(&mut self.expcmdsn, &mut self.maxcmdsn, expcmdsn, maxcmdsn);
    }
}

fn update_window_edges(exp: &mut u32, max: &mut u32, expcmdsn: u32, maxcmdsn: u32) {
    // Invalid ExpCmdSN/MaxCmdSN pairs MUST be ignored. Advertisements can
    // be stale; neither edge of the window is allowed to move backwards.
    if maxcmdsn.wrapping_add(1).wrapping_sub(expcmdsn) >= 0x8000_0000 {
        return;
    }
    if serial_after(expcmdsn, *exp) {
        *exp = expcmdsn;
    }
    if serial_after(maxcmdsn, *max) {
        *max = maxcmdsn;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn burst_integrity_is_deferred_until_final_login_response() {
        let mut params = NegotiatedParams::initial(&SessionOptions::default());
        apply_params(&mut params, &[("maxburstlength".into(), "65536".into())]).unwrap();
        assert!(params.first_burst_length > params.max_burst_length);
        apply_params(&mut params, &[("firstburstlength".into(), "65536".into())]).unwrap();
        assert_eq!(params.first_burst_length, params.max_burst_length);
    }
    #[test]
    fn serial_comparison_including_wraparound() {
        assert!(serial_after(1, u32::MAX));
        assert!(!serial_after(u32::MAX, 1));
        assert!(!serial_after(1, 1));
    }

    #[test]
    fn window_updates_ignore_stale_invalid_and_accept_wrapped_pairs() {
        let (mut exp, mut max) = (10, 20);
        update_window_edges(&mut exp, &mut max, 9, 19);
        assert_eq!((exp, max), (10, 20));
        update_window_edges(&mut exp, &mut max, 30, 28); // invalid pair.
        assert_eq!((exp, max), (10, 20));
        update_window_edges(&mut exp, &mut max, 21, 20); // closed window.
        assert_eq!((exp, max), (21, 20));
        update_window_edges(&mut exp, &mut max, 21, 25);
        assert_eq!((exp, max), (21, 25));
        (exp, max) = (u32::MAX - 2, u32::MAX);
        update_window_edges(&mut exp, &mut max, 0, 3);
        assert_eq!((exp, max), (0, 3));
    }
}
