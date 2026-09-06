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
//!
//! The write fence (C18) cannot wrap pasta itself: the kernel denies every
//! mount-topology syscall (mount, umount, pivot_root) to a process inside a
//! Landlock domain, and no rule can grant them back (see the `sb_mount`,
//! `sb_umount`, and `sb_pivotroot` hooks in the kernel's
//! `security/landlock/fs.c`). pasta needs exactly those syscalls to sandbox
//! itself. The fence therefore moves inside the namespace: when the fence is
//! on, the command after pasta's `--` is `gh-klon __fence-exec`, which
//! applies the same ruleset and then execs the command. pasta runs unfenced;
//! the command runs under the same fence as without `--netns`. The DNS
//! rescue of a loopback-stub host rides on that carrier: without the fence
//! (`KLON_NO_FENCE=1`) the wrapper carries no `__fence-exec`, so it carries
//! no rescue listener either.

use crate::envelope::{env, Envelope, Part};
use crate::{probe, Error, Result};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

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

/// The value of the port mapping `-t` option for one address:
/// `127.0.0.2/3000,5173`. An empty list maps nothing, which is the documented
/// spec `none`. The value is its own argv element: getopt would read a space
/// inside `-t <value>` as the first character of the value.
fn port_value(ip: &str, ports: &[u16]) -> String {
    if ports.is_empty() {
        return "none".to_string();
    }
    let list = ports
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(",");
    format!("{ip}/{list}")
}

/// The inner fence carrier: when the write fence is on, the command after
/// pasta's `--` is `exe __fence-exec <klon> [--cgroup <dir>] -- <cmd>`.
struct FencedExec {
    /// The klon binary itself, so the wrapper never depends on `PATH`.
    exe: PathBuf,
    /// The klon directory, for the fence's allow set.
    klon: PathBuf,
    /// The scope cgroup (C20) the fence may open for its `cgroup.procs` rule.
    /// The join itself happens outside pasta, so the rule only keeps the
    /// moved fence identical to the fence a run without `--netns` builds.
    cgroup: Option<PathBuf>,
    /// The DNS rescue plan: the stub address to bind inside the namespace,
    /// and the routable upstreams to relay to (see `rescue_from`).
    dns: Option<(IpAddr, Vec<IpAddr>)>,
}

/// Every `nameserver` address of a `resolv.conf` text, in file order.
fn parse_nameservers(text: &str) -> Vec<IpAddr> {
    text.lines()
        .map(str::split_whitespace)
        .filter_map(|mut words| match (words.next(), words.next()) {
            (Some("nameserver"), Some(addr)) => addr.parse().ok(),
            _ => None,
        })
        .collect()
}

/// The DNS rescue plan: when every nameserver of the host resolver is a
/// loopback address, the namespace cannot reach it (a loopback packet stays
/// on the namespace's own `lo`), so resolution would fail inside. When the
/// alternate resolver file holds routable nameservers, klon binds the first
/// stub address inside the namespace and relays to those.
fn rescue_from(host: &str, alt: &str) -> Option<(IpAddr, Vec<IpAddr>)> {
    let stubs = parse_nameservers(host);
    if stubs.is_empty() || !stubs.iter().all(IpAddr::is_loopback) {
        return None;
    }
    let routable: Vec<IpAddr> = parse_nameservers(alt)
        .into_iter()
        .filter(|ip| !ip.is_loopback())
        .collect();
    if routable.is_empty() {
        return None;
    }
    Some((stubs[0], routable))
}

/// The DNS rescue plan for this host, from `/etc/resolv.conf` and the
/// systemd-resolved file beside it. `None` needs no rescue.
fn dns_rescue() -> Option<(IpAddr, Vec<IpAddr>)> {
    let host = std::fs::read_to_string("/etc/resolv.conf").ok()?;
    let alt = std::fs::read_to_string("/run/systemd/resolve/resolv.conf").ok()?;
    rescue_from(&host, &alt)
}

/// The envelope part: pasta with the host's network configuration, the port
/// mapping for the klon's address, and the command after `--`. With the
/// write fence on, the command is the `__fence-exec` re-exec.
fn part(ip: &str, ports: &[u16], fenced: Option<FencedExec>) -> Part {
    let mut wrapper = vec![
        "pasta".to_string(),
        "--config-net".to_string(),
        "-t".to_string(),
        port_value(ip, ports),
        "--".to_string(),
    ];
    if let Some(fenced) = fenced {
        wrapper.push(fenced.exe.to_string_lossy().into_owned());
        wrapper.push("__fence-exec".to_string());
        wrapper.push(fenced.klon.to_string_lossy().into_owned());
        if let Some(dir) = fenced.cgroup {
            wrapper.push("--cgroup".to_string());
            wrapper.push(dir.to_string_lossy().into_owned());
        }
        if let Some((stub, upstreams)) = fenced.dns {
            wrapper.push("--dns-rescue".to_string());
            // The stub rides with its port: `--dns-rescue` takes a socket
            // address, and a resolv.conf nameserver is a bare address. The
            // socket address display brackets IPv6, so `::1` keeps parsing.
            wrapper.push(SocketAddr::new(stub, 53).to_string());
            wrapper.push("--dns-upstream".to_string());
            // One argv word with a comma list: the same compactness keeps the
            // wrapper short, and clap splits it on the commas.
            let list = upstreams
                .iter()
                .map(IpAddr::to_string)
                .collect::<Vec<_>>()
                .join(",");
            wrapper.push(list);
        }
        wrapper.push("--".to_string());
    }
    Part {
        vars: Vec::new(),
        wrapper,
    }
}

/// Turn the pasta wrapper on for `envelope`. A host without pasta prints one
/// line and leaves the envelope as it is, so the command runs as before.
///
/// `cgroup` is the scope cgroup (C20) the fence names in its allow set. When
/// the write fence is on, `enable` takes it out of the envelope: pasta must
/// start unfenced (a Landlock domain denies pasta's own mount sandbox), and
/// the fence moves into the wrapper as the `__fence-exec` re-exec.
pub fn enable(envelope: &mut Envelope, ports: &[u16], cgroup: Option<&Path>) -> Result<()> {
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
    let fenced = envelope.fence.take().is_some();
    let exec = if fenced {
        let dns = dns_rescue();
        if let Some((stub, upstreams)) = &dns {
            if std::env::var_os("KLON_DEBUG").is_some_and(|v| !v.is_empty() && v != "0") {
                let list = upstreams
                    .iter()
                    .map(IpAddr::to_string)
                    .collect::<Vec<_>>()
                    .join(" ");
                eprintln!(
                    "klon: netns: the host resolver is a loopback stub, relaying {stub} to {list}"
                );
            }
        }
        Some(FencedExec {
            exe: std::env::current_exe().map_err(Error::io("find the klon binary"))?,
            klon: envelope.klon.clone(),
            cgroup: cgroup.map(Path::to_path_buf),
            dns,
        })
    } else {
        None
    };
    envelope.netns = Some(part(&ip, ports, exec));
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
        assert_eq!(port_value("127.0.0.2", &[]), "none");
    }

    #[test]
    fn the_wrapper_carries_the_address_and_the_port_list() {
        let part = part("127.0.0.3", &[3000, 5173], None);
        assert!(part.vars.is_empty());
        assert_eq!(
            part.wrapper,
            vec![
                "pasta".to_string(),
                "--config-net".to_string(),
                "-t".to_string(),
                "127.0.0.3/3000,5173".to_string(),
                "--".to_string(),
            ]
        );
        // No word before pasta's `--` holds a space: getopt would read it as
        // part of the option value.
        assert!(part.wrapper.iter().take(4).all(|word| !word.contains(' ')));
    }

    #[test]
    fn the_fence_moves_inside_the_namespace_as_the_reexec() {
        let fenced = FencedExec {
            exe: PathBuf::from("/opt/gh-klon"),
            klon: PathBuf::from("/repo/.klon-work/feature"),
            cgroup: Some(PathBuf::from("/sys/fs/cgroup/klon-1")),
            dns: None,
        };
        let part = part("127.0.0.2", &[3000], Some(fenced));
        assert_eq!(
            part.wrapper,
            vec![
                "pasta".to_string(),
                "--config-net".to_string(),
                "-t".to_string(),
                "127.0.0.2/3000".to_string(),
                "--".to_string(),
                "/opt/gh-klon".to_string(),
                "__fence-exec".to_string(),
                "/repo/.klon-work/feature".to_string(),
                "--cgroup".to_string(),
                "/sys/fs/cgroup/klon-1".to_string(),
                "--".to_string(),
            ]
        );
    }

    #[test]
    fn the_parser_reads_the_nameservers_of_a_resolv_conf() {
        assert_eq!(
            parse_nameservers(
                "options edns0 trust-ad\nnameserver 127.0.0.53\n# a comment\nnameserver ::1\nsearch lan\n"
            ),
            vec![
                "127.0.0.53".parse::<IpAddr>().unwrap(),
                "::1".parse::<IpAddr>().unwrap(),
            ]
        );
        assert!(parse_nameservers("search lan\n").is_empty());
    }

    #[test]
    fn the_rescue_needs_a_stub_host_and_a_routable_alternate() {
        let stub_host = "nameserver 127.0.0.53\n";
        let direct_host = "nameserver 10.206.0.2\n";
        let alt = "nameserver 10.206.0.2\nnameserver 127.0.0.53\n";
        // A loopback-only host resolver with routable upstreams: the rescue.
        assert_eq!(
            rescue_from(stub_host, alt),
            Some((
                "127.0.0.53".parse().unwrap(),
                vec!["10.206.0.2".parse().unwrap()]
            ))
        );
        // A direct host resolver needs no rescue.
        assert_eq!(rescue_from(direct_host, alt), None);
        // Loopback-only upstreams cannot serve a namespace.
        assert_eq!(rescue_from(stub_host, "nameserver 127.0.0.53\n"), None);
        // No nameserver at all: nothing to bind.
        assert_eq!(rescue_from("search lan\n", alt), None);
    }

    #[test]
    fn the_wrapper_carries_the_dns_rescue_plan() {
        let fenced = FencedExec {
            exe: PathBuf::from("/opt/gh-klon"),
            klon: PathBuf::from("/repo/.klon-work/feature"),
            cgroup: None,
            dns: Some((
                "127.0.0.53".parse().unwrap(),
                vec!["10.206.0.2".parse().unwrap(), "10.206.0.3".parse().unwrap()],
            )),
        };
        let part = part("127.0.0.2", &[3000], Some(fenced));
        let words = part.wrapper;
        let start = words
            .iter()
            .position(|word| word == "--dns-rescue")
            .unwrap();
        assert_eq!(
            &words[start..start + 4],
            &[
                "--dns-rescue".to_string(),
                "127.0.0.53:53".to_string(),
                "--dns-upstream".to_string(),
                "10.206.0.2,10.206.0.3".to_string(),
            ]
        );
        assert!(words.iter().all(|word| !word.contains(' ')));
    }

    #[test]
    fn an_ipv6_stub_rides_with_brackets() {
        let fenced = FencedExec {
            exe: PathBuf::from("/opt/gh-klon"),
            klon: PathBuf::from("/repo/.klon-work/feature"),
            cgroup: None,
            dns: Some(("::1".parse().unwrap(), vec!["::1".parse().unwrap()])),
        };
        let words = part("127.0.0.2", &[3000], Some(fenced)).wrapper;
        assert!(words.contains(&"[::1]:53".to_string()));
    }
}
