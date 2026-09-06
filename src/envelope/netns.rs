//! The `--netns` part (C23, R21): wrap the command in a `pasta` network
//! namespace, so two klons can run the same port at the same time.
//!
//! pasta creates a rootless namespace with the host's addresses and routes
//! (`--config-net`, which keeps outbound traffic working), and maps a list of
//! TCP ports from the klon's own loopback address into it. A server bound to
//! `0.0.0.0:3000` inside the namespace answers on the host at
//! `<KLON_IP>:3000`, and a second klon binds its own address without
//! `EADDRINUSE`. UDP ports with the numbers of mapped TCP ports come along
//! without a separate `-u` setting.
//!
//! A host without pasta gets one stderr line and the command runs on the host
//! network as before. `doctor` reports the tool and the install line.

use crate::envelope::{env, Envelope, Part};
use crate::{probe, Error, Result};

/// The ports pasta maps when neither `--netns-ports` nor `.klon.toml` names
/// any. These are the ports a web frontend, a Vite dev server, and two common
/// backend ports use.
pub const DEFAULT_PORTS: &[u16] = &[3000, 5173, 8000, 8080];

/// The one line a host without pasta gets under `run --netns`.
pub const ABSENT: &str = "klon: pasta absent, running without a network namespace";

/// The port list: the flag wins, then the config, then the default.
pub fn ports(flag: Option<&[u16]>, config: Option<&[u16]>) -> Vec<u16> {
    flag.or(config).unwrap_or(DEFAULT_PORTS).to_vec()
}

/// The port mapping `-t` value for one address: `127.0.0.2/3000,5173`. An
/// empty list maps nothing, which is the documented spec `none`.
fn port_spec(ip: &str, ports: &[u16]) -> String {
    if ports.is_empty() {
        return "-t none".to_string();
    }
    let list = ports
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(",");
    format!("-t {ip}/{list}")
}

/// The envelope part: pasta with the host's network configuration, the port
/// mapping for the klon's address, and the command after `--`.
fn part(ip: &str, ports: &[u16]) -> Part {
    Part {
        vars: Vec::new(),
        wrapper: vec![
            "pasta".to_string(),
            "--config-net".to_string(),
            port_spec(ip, ports),
            "--".to_string(),
        ],
    }
}

/// Turn the pasta wrapper on for `envelope`. A host without pasta prints one
/// line and leaves the envelope as it is, so the command runs as before.
pub fn enable(envelope: &mut Envelope, ports: &[u16]) -> Result<()> {
    let ip = envelope
        .var("KLON_IP")
        .ok_or_else(|| {
            Error::klon(format!(
                "{} holds no KLON_IP",
                env::file(&envelope.klon).display()
            ))
        })?
        .to_string();
    if probe::tool_path("pasta").is_none() {
        eprintln!("{ABSENT}");
        return Ok(());
    }
    envelope.netns = Some(part(&ip, ports));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_list_answers_when_nothing_else_does() {
        assert_eq!(ports(None, None), DEFAULT_PORTS.to_vec());
    }

    #[test]
    fn the_flag_wins_over_the_config_and_the_config_over_the_default() {
        assert_eq!(
            ports(Some(&[8080]), Some(&[9000])),
            vec![8080],
            "the flag must win"
        );
        assert_eq!(ports(None, Some(&[9000])), vec![9000]);
    }

    #[test]
    fn an_empty_flag_means_no_forwarded_ports() {
        assert_eq!(ports(Some(&[]), None), Vec::<u16>::new());
        assert_eq!(port_spec("127.0.0.2", &[]), "-t none");
    }

    #[test]
    fn the_wrapper_carries_the_address_and_the_port_list() {
        let part = part("127.0.0.3", &[3000, 5173]);
        assert!(part.vars.is_empty());
        assert_eq!(
            part.wrapper,
            vec![
                "pasta".to_string(),
                "--config-net".to_string(),
                "-t 127.0.0.3/3000,5173".to_string(),
                "--".to_string(),
            ]
        );
    }
}
