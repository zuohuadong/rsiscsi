use crate::Error;
use serde::{Deserialize, Serialize};
use std::{fmt, time::Duration};

/// Portal address, kept separate from the IQN.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetAddr {
    pub host: String,
    pub port: u16,
}

/// Credentials deliberately redact the secret from Debug output.
#[derive(Clone, PartialEq, Eq)]
pub struct ChapCredentials {
    pub user: String,
    pub secret: String,
}

impl fmt::Debug for ChapCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChapCredentials")
            .field("user", &self.user)
            .field("secret", &"[REDACTED]")
            .finish()
    }
}

/// A structured target; no load-time normalization of any field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub addr: TargetAddr,
    pub iqn: String,
    pub lun: u64,
    pub chap: Option<ChapCredentials>,
}

/// A lossless URL for import/edit flows. `as_str()` preserves even omitted
/// default ports/LUNs, case and percent sequences byte-for-byte. Use `target()`
/// for the structured connection parameters; persist the original, not a
/// canonical reserialization. Debug never prints the credential-bearing URL.
pub struct TargetUrl {
    target: Target,
    original: String,
}

impl TargetUrl {
    pub fn parse(input: &str, fallback: Option<&TargetAddr>) -> Result<Self, Error> {
        Ok(Self {
            target: Target::parse_url(input, fallback)?,
            original: input.to_owned(),
        })
    }
    pub fn target(&self) -> &Target {
        &self.target
    }
    pub fn into_target(self) -> Target {
        self.target
    }
    pub fn as_str(&self) -> &str {
        &self.original
    }
}

impl fmt::Debug for TargetUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TargetUrl")
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

impl Target {
    /// Parse iscsi://[user%secret@]host[:port]/IQN[/LUN].
    ///
    /// The legacy IQN-only form requires an explicit fallback portal. Colons
    /// are never used to split an IQN. Inputs are not URL-decoded or normalized.
    pub fn parse_url(input: &str, fallback: Option<&TargetAddr>) -> Result<Self, Error> {
        let body = input
            .strip_prefix("iscsi://")
            .ok_or_else(|| Error::Config("expected iscsi:// URL".into()))?;
        if body.starts_with("iqn.") && !body.contains('/') {
            return Ok(Self {
                addr: fallback
                    .cloned()
                    .ok_or_else(|| Error::Config("IQN-only URL needs a portal".into()))?,
                iqn: body.to_owned(),
                lun: 0,
                chap: None,
            });
        }
        let (authority, path) = body
            .split_once('/')
            .ok_or_else(|| Error::Config("missing IQN path".into()))?;
        let (authority, chap) = if let Some((credentials, authority)) = authority.rsplit_once('@') {
            let (user, secret) = credentials
                .split_once('%')
                .ok_or_else(|| Error::Config("CHAP URL needs user%secret".into()))?;
            (
                authority,
                Some(ChapCredentials {
                    user: user.into(),
                    secret: secret.into(),
                }),
            )
        } else {
            (authority, None)
        };
        let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
            let (host, suffix) = rest
                .split_once(']')
                .ok_or_else(|| Error::Config("unterminated IPv6 address".into()))?;
            let port = if suffix.is_empty() {
                3260
            } else {
                parse_port(
                    suffix
                        .strip_prefix(':')
                        .ok_or_else(|| Error::Config("invalid IPv6 authority".into()))?,
                )?
            };
            (host.to_owned(), port)
        } else if let Some((host, port)) = authority.split_once(':') {
            (host.to_owned(), parse_port(port)?)
        } else {
            (authority.to_owned(), 3260)
        };
        let (iqn, lun) = match path.rsplit_once('/') {
            Some((iqn, last)) if !last.is_empty() && last.bytes().all(|b| b.is_ascii_digit()) => (
                iqn,
                last.parse()
                    .map_err(|_| Error::Config("invalid LUN".into()))?,
            ),
            _ => (path, 0),
        };
        if host.is_empty() || iqn.is_empty() {
            return Err(Error::Config("host and IQN must not be empty".into()));
        }
        Ok(Self {
            addr: TargetAddr { host, port },
            iqn: iqn.to_owned(),
            lun,
            chap,
        })
    }

    /// Serialize a canonical URL, preserving the IQN bytes exactly.
    ///
    /// The returned value includes credentials when present; never log it.
    pub fn to_url(&self) -> String {
        let host = if self.addr.host.contains(':') {
            format!("[{}]", self.addr.host)
        } else {
            self.addr.host.clone()
        };
        let auth = self
            .chap
            .as_ref()
            .map_or_else(String::new, |c| format!("{}%{}@", c.user, c.secret));
        format!(
            "iscsi://{auth}{host}:{}/{}/{}",
            self.addr.port, self.iqn, self.lun
        )
    }
}

fn parse_port(value: &str) -> Result<u16, Error> {
    value
        .parse::<u16>()
        .ok()
        .filter(|p| *p != 0)
        .ok_or_else(|| Error::Config("invalid TCP port".into()))
}

/// Connection and negotiation limits. Digests default off.
#[derive(Debug, Clone)]
pub struct SessionOptions {
    pub initiator_name: String,
    pub queue_depth: usize,
    pub header_digest: bool,
    pub data_digest: bool,
    pub max_recv_data_segment_length: u32,
    pub max_burst_length: u32,
    pub first_burst_length: u32,
    pub nodelay: bool,
    pub connect_timeout: Duration,
    pub io_timeout: Duration,
}

impl Default for SessionOptions {
    fn default() -> Self {
        let host = hostname::get()
            .ok()
            .and_then(|h| h.into_string().ok())
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| "localhost".into());
        Self {
            initiator_name: format!("iqn.2026-11.local.iscsi-rs:{host}:{}", std::process::id()),
            queue_depth: 128,
            header_digest: false,
            data_digest: false,
            max_recv_data_segment_length: 262_144,
            max_burst_length: 1_048_576,
            first_burst_length: 262_144,
            nodelay: true,
            connect_timeout: Duration::from_secs(10),
            io_timeout: Duration::from_secs(30),
        }
    }
}

impl SessionOptions {
    pub(crate) fn validate(&self) -> Result<(), Error> {
        if self.queue_depth == 0
            || self.queue_depth > 65_536
            || !(512..=0x00ff_ffff).contains(&self.max_recv_data_segment_length)
            || !(512..=0x00ff_ffff).contains(&self.max_burst_length)
            || !(512..=self.max_burst_length).contains(&self.first_burst_length)
            || self.connect_timeout.is_zero()
            || self.io_timeout.is_zero()
        {
            return Err(Error::Config(
                "invalid queue, segment, burst or timeout limit".into(),
            ));
        }
        validate_text("InitiatorName", &self.initiator_name)
    }
}

pub(crate) fn validate_text(name: &str, value: &str) -> Result<(), Error> {
    if value.is_empty() || value.bytes().any(|b| b == 0 || b == b'\n' || b == b'\r') {
        return Err(Error::Config(format!("invalid {name}")));
    }
    Ok(())
}

/// Actual operational parameters selected by the target.
#[derive(Debug, Clone, Serialize)]
pub struct NegotiatedParams {
    pub header_digest: bool,
    pub data_digest: bool,
    /// Initiator receive limit.
    pub max_recv_data_segment_length: u32,
    /// Target receive limit, independent of the initiator receive limit.
    pub target_max_recv_data_segment_length: u32,
    pub max_burst_length: u32,
    pub first_burst_length: u32,
    pub initial_r2t: bool,
    pub immediate_data: bool,
    pub default_time2wait: u32,
    pub default_time2retain: u32,
    pub error_recovery_level: u32,
    pub max_connections: u32,
    pub max_outstanding_r2t: u32,
}

impl NegotiatedParams {
    pub(crate) fn initial(opts: &SessionOptions) -> Self {
        Self {
            header_digest: false,
            data_digest: false,
            max_recv_data_segment_length: opts.max_recv_data_segment_length,
            target_max_recv_data_segment_length: 8192,
            max_burst_length: opts.max_burst_length,
            first_burst_length: opts.first_burst_length,
            initial_r2t: true,
            immediate_data: true,
            default_time2wait: 2,
            default_time2retain: 20,
            error_recovery_level: 0,
            max_connections: 1,
            max_outstanding_r2t: 1,
        }
    }
}

/// One target and its advertised portals.
#[derive(Debug, Clone, Serialize)]
pub struct DiscoveredTarget {
    pub iqn: String,
    pub addresses: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    const IQN: &str = "iqn.2004-04.com.qnap:ts-464c2:iscsi.zhd-macmini-0.873836";

    #[test]
    fn historic_iqn_only_url_preserves_colons() {
        let portal = TargetAddr {
            host: "10.1.10.1".into(),
            port: 3260,
        };
        let t = Target::parse_url(&format!("iscsi://{IQN}"), Some(&portal)).unwrap();
        assert_eq!(t.iqn.as_bytes(), IQN.as_bytes());
        assert_eq!(t.addr, portal);
        assert_eq!(t.lun, 0);
    }

    #[test]
    fn structured_urls_round_trip_iqn_bytes() {
        for url in [
            format!("iscsi://10.1.10.1:3260/{IQN}/1"),
            format!("iscsi://[::1]:3260/{IQN}/0"),
            format!("iscsi://user%password@host:3260/{IQN}/12"),
            "iscsi://host:3260/iqn.keep:UPPER:%2F:=/part/5".into(),
        ] {
            let target = Target::parse_url(&url, None).unwrap();
            assert_eq!(target.to_url(), url);
            assert_eq!(Target::parse_url(&target.to_url(), None).unwrap(), target);
        }
        assert_eq!(
            Target::parse_url(&format!("iscsi://host/{IQN}"), None)
                .unwrap()
                .lun,
            0
        );
        assert!(Target::parse_url("iscsi://host:bad/iqn.foo", None).is_err());
        assert!(Target::parse_url("iscsi://[::1/iqn.foo", None).is_err());
    }

    #[test]
    fn imported_url_round_trips_every_byte_without_normalization() {
        let portal = TargetAddr {
            host: "10.1.10.1".into(),
            port: 3260,
        };
        for url in [
            format!("iscsi://{IQN}"),
            format!("iscsi://HOST/{IQN}"),
            format!("iscsi://[::1]:03260/{IQN}/001"),
            "iscsi://user%secret@host/iqn.keep:Case:%2f:==".into(),
        ] {
            let parsed = TargetUrl::parse(&url, Some(&portal)).unwrap();
            assert_eq!(parsed.as_str().as_bytes(), url.as_bytes());
        }
    }
}
