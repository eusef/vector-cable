//! Finding the bridge: `10.42.0.1` (its own access point), then DNS-SD, then
//! `vectorbridge.local`. The only probe traffic is a `status` request, which
//! never moves the peer slot.

use std::net::{IpAddr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::Duration;

use crate::protocol::{Reply, Request, CONTROL_PORT};

pub const AP_ADDRESS: &str = "10.42.0.1";
pub const LOCAL_NAME: &str = "vectorbridge.local";
pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_millis(800);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Via {
    /// The bridge's own access point address.
    AccessPoint,
    /// A DNS-SD record. None is defined yet (Pi-side ask P5), so this step finds nothing.
    DnsSd,
    /// The Pi's mDNS responder, through the system resolver.
    LocalName,
    /// `--bridge HOST`.
    Given,
}

impl Via {
    pub fn word(self) -> &'static str {
        match self {
            Via::AccessPoint => "access point",
            Via::DnsSd => "DNS-SD",
            Via::LocalName => "vectorbridge.local",
            Via::Given => "given",
        }
    }
}

/// The search order.
pub fn order() -> [Via; 3] {
    [Via::AccessPoint, Via::DnsSd, Via::LocalName]
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    /// What the operator typed or the step tried, for display and `setup`.
    pub host: String,
    pub addr: IpAddr,
    pub via: Via,
}

/// DNS-SD lookup. Returns `None` until P5 defines the service record
/// (for example `_vectorbridge._udp`); there is nothing to query yet.
pub fn dns_sd() -> Option<(String, IpAddr)> {
    None
}

pub fn resolve(host: &str) -> std::io::Result<Vec<IpAddr>> {
    let addrs: Vec<IpAddr> = (host, CONTROL_PORT)
        .to_socket_addrs()?
        .map(|a| a.ip())
        .collect();
    // Prefer IPv4: the bridge serves on IPv4 and its mDNS name may also publish a link-local v6.
    let mut v4: Vec<IpAddr> = addrs.iter().copied().filter(IpAddr::is_ipv4).collect();
    v4.extend(addrs.into_iter().filter(|a| !a.is_ipv4()));
    Ok(v4)
}

/// Sends `status` to `addr:control_port` and waits for any `ok` reply.
pub fn probe(addr: IpAddr, control_port: u16, timeout: Duration) -> bool {
    let bind: SocketAddr = if addr.is_ipv4() {
        "0.0.0.0:0".parse().unwrap()
    } else {
        "[::]:0".parse().unwrap()
    };
    let Ok(socket) = UdpSocket::bind(bind) else {
        return false;
    };
    if socket.set_read_timeout(Some(timeout)).is_err() {
        return false;
    }
    if socket
        .send_to(
            &Request::Status.to_bytes(),
            SocketAddr::new(addr, control_port),
        )
        .is_err()
    {
        return false;
    }
    let mut buf = [0u8; 4096];
    match socket.recv_from(&mut buf) {
        Ok((n, from)) if from.ip() == addr => Reply::parse(&buf[..n]).is_ok(),
        _ => false,
    }
}

/// Tries each step in order and returns the first bridge that answers
/// `status`. With `given`, only that host is tried.
pub fn discover(given: Option<&str>, control_port: u16, timeout: Duration) -> Option<Found> {
    discover_with(
        given,
        &|host| resolve(host).unwrap_or_default(),
        &dns_sd,
        &|addr| probe(addr, control_port, timeout),
    )
}

/// [`discover`] with the resolver, the DNS-SD lookup and the prober passed
/// in, so the search order can be tested without a network.
pub fn discover_with(
    given: Option<&str>,
    resolve: &dyn Fn(&str) -> Vec<IpAddr>,
    dns_sd: &dyn Fn() -> Option<(String, IpAddr)>,
    probe: &dyn Fn(IpAddr) -> bool,
) -> Option<Found> {
    if let Some(host) = given {
        return resolve(host)
            .into_iter()
            .find(|a| probe(*a))
            .map(|addr| Found {
                host: host.to_string(),
                addr,
                via: Via::Given,
            });
    }
    for via in order() {
        let candidates: Vec<(String, IpAddr)> = match via {
            Via::AccessPoint => vec![(AP_ADDRESS.to_string(), AP_ADDRESS.parse().unwrap())],
            Via::DnsSd => dns_sd().into_iter().collect(),
            Via::LocalName => resolve(LOCAL_NAME)
                .into_iter()
                .map(|a| (LOCAL_NAME.to_string(), a))
                .collect(),
            Via::Given => Vec::new(),
        };
        for (host, addr) in candidates {
            if probe(addr) {
                return Some(Found { host, addr, via });
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_order_is_ap_then_dns_sd_then_local_name() {
        assert_eq!(order(), [Via::AccessPoint, Via::DnsSd, Via::LocalName]);
        assert_eq!(dns_sd(), None, "no record defined until P5");
    }

    #[test]
    fn discover_tries_ap_then_dns_sd_then_local_name_and_stops_at_the_first_answer() {
        use std::cell::RefCell;
        let ap: IpAddr = AP_ADDRESS.parse().unwrap();
        let sd: IpAddr = "192.0.2.20".parse().unwrap();
        let local: IpAddr = "192.0.2.30".parse().unwrap();
        let log = RefCell::new(Vec::new());
        let resolve = |host: &str| {
            log.borrow_mut().push(format!("resolve {host}"));
            if host == LOCAL_NAME {
                vec![local]
            } else {
                vec![]
            }
        };
        let dns_sd = || {
            log.borrow_mut().push("dns-sd".to_string());
            Some(("bridge._vbridge._udp.local".to_string(), sd))
        };
        // Only the .local name answers.
        let probe = |addr: IpAddr| {
            log.borrow_mut().push(format!("probe {addr}"));
            addr == local
        };
        let found = discover_with(None, &resolve, &dns_sd, &probe).unwrap();
        assert_eq!(found.via, Via::LocalName);
        assert_eq!(found.addr, local);
        assert_eq!(found.host, LOCAL_NAME);
        assert_eq!(
            *log.borrow(),
            vec![
                format!("probe {ap}"),
                "dns-sd".to_string(),
                format!("probe {sd}"),
                format!("resolve {LOCAL_NAME}"),
                format!("probe {local}"),
            ]
        );

        // The AP address answers: nothing after it is tried.
        log.borrow_mut().clear();
        let probe_ap = |addr: IpAddr| {
            log.borrow_mut().push(format!("probe {addr}"));
            addr == ap
        };
        let found = discover_with(None, &resolve, &dns_sd, &probe_ap).unwrap();
        assert_eq!(found.via, Via::AccessPoint);
        assert_eq!(*log.borrow(), vec![format!("probe {ap}")]);

        // A given host skips the search entirely.
        log.borrow_mut().clear();
        let resolve_given = |host: &str| {
            log.borrow_mut().push(format!("resolve {host}"));
            vec![local]
        };
        let found = discover_with(Some("mybridge"), &resolve_given, &dns_sd, &probe).unwrap();
        assert_eq!(found.via, Via::Given);
        assert_eq!(found.host, "mybridge");
        assert_eq!(
            *log.borrow(),
            vec!["resolve mybridge".to_string(), format!("probe {local}")]
        );

        // Nobody answers: None after every step.
        let none = discover_with(None, &resolve, &dns_sd, &|_| false);
        assert_eq!(none, None);
    }

    #[test]
    fn probe_answers_true_for_a_status_reply_and_false_for_silence() {
        let fake = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = fake.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let mut buf = [0u8; 256];
            let (n, from) = fake.recv_from(&mut buf).unwrap();
            assert_eq!(&buf[..n], br#"{"cmd":"status"}"#);
            fake.send_to(br#"{"ok":true,"uptime_s":1}"#, from).unwrap();
        });
        assert!(probe(
            "127.0.0.1".parse().unwrap(),
            port,
            Duration::from_secs(2)
        ));
        server.join().unwrap();

        let silent = UdpSocket::bind("127.0.0.1:0").unwrap();
        let dead_port = silent.local_addr().unwrap().port();
        assert!(!probe(
            "127.0.0.1".parse().unwrap(),
            dead_port,
            Duration::from_millis(200)
        ));

        let found = discover(Some("127.0.0.1"), dead_port, Duration::from_millis(100));
        assert_eq!(found, None);
    }
}
