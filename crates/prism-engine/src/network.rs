//! Multi-network downloads: spread a download's connections over several network adapters
//! (Wi-Fi, Ethernet, a tethered phone, …) so their bandwidth adds up.
//!
//! Every connection of a segmented HTTP download is pinned to one [`Network`]. Work stealing in
//! the segment table does the balancing: connections on a fast network finish their ranges
//! sooner and take more work, and slow tails are raced by idle connections on other networks.
//!
//! Pinning a socket to an adapter:
//! * Linux and macOS bind the socket to the interface itself (`SO_BINDTODEVICE` /
//!   `IP_BOUND_IF`) and drop DNS answers of a family the adapter has no address for. On Linux, binding only to the adapter's IP would still send packets out the
//!   default route, where the network that does not own that IP drops them.
//! * Windows routes by source address, so the socket is bound to the adapter's IPv4 address
//!   (or IPv6 when it has no IPv4) and DNS answers are filtered to that address family.
//!
//! An empty [`NetworkSet`] (the default) means "use the operating system's default route",
//! exactly as before.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::limiter::RateLimiter;

/// Consecutive failures to connect to a server after which a network is taken out of
/// rotation for that server.
const BENCH_AFTER_FAILURES: u32 = 3;
/// How long a failing network is not given new connections to that server.
const BENCH_FOR: Duration = Duration::from_secs(30);

/// The server a connection goes to (`host:port`): a network may reach one server and not
/// another (an IPv6-only tether, a filtered network).
pub(crate) fn server_of(url: &url::Url) -> String {
    format!(
        "{}:{}",
        url.host_str().unwrap_or_default(),
        url.port_or_known_default().unwrap_or_default()
    )
}

/// How well a network reaches one server.
#[derive(Debug)]
struct Reach {
    /// Failures in a row, each within [`BENCH_FOR`] of the one before.
    failures: u32,
    last_failure: Instant,
    benched_until: Option<Instant>,
}

/// A network adapter of this computer, as reported by the operating system.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkInterface {
    /// Adapter name (`en0`, `wlan0`, `Wi-Fi`, …): the stable identifier of a network.
    pub name: String,
    /// Non-loopback, non-link-local addresses of the adapter.
    pub addrs: Vec<IpAddr>,
}

impl NetworkInterface {
    /// The address a socket binds to where the OS routes by source address.
    pub fn preferred_addr(&self) -> Option<IpAddr> {
        self.addrs
            .iter()
            .find(|a| a.is_ipv4())
            .or_else(|| self.addrs.first())
            .copied()
    }

    /// Whether it has an IPv4 / IPv6 address (DNS answers of a missing family can't be
    /// reached through it).
    pub fn families(&self) -> (bool, bool) {
        (
            self.addrs.iter().any(IpAddr::is_ipv4),
            self.addrs.iter().any(IpAddr::is_ipv6),
        )
    }
}

fn usable_addr(ip: &IpAddr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
        return false;
    }
    match ip {
        IpAddr::V4(v4) => !v4.is_link_local(),
        // fe80::/10
        IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) != 0xfe80,
    }
}

/// Lists the adapters that have a usable (non-loopback, non-link-local) address, in OS order.
pub fn list_interfaces() -> Vec<NetworkInterface> {
    interfaces(false)
}

/// Like [`list_interfaces`]; `with_loopback` also lists loopback adapters (a network may be
/// configured on one, which is how the tests run two "networks" on one machine).
fn interfaces(with_loopback: bool) -> Vec<NetworkInterface> {
    let mut out: Vec<NetworkInterface> = Vec::new();
    let Ok(ifaces) = if_addrs::get_if_addrs() else {
        return out;
    };
    for i in ifaces {
        let ip = i.ip();
        let usable = if i.is_loopback() {
            with_loopback
        } else {
            usable_addr(&ip)
        };
        if !usable || i.oper_status == if_addrs::IfOperStatus::Down {
            continue;
        }
        match out.iter_mut().find(|n| n.name == i.name) {
            Some(n) => {
                if !n.addrs.contains(&ip) {
                    n.addrs.push(ip)
                }
            }
            None => out.push(NetworkInterface {
                name: i.name,
                addrs: vec![ip],
            }),
        }
    }
    out
}

/// What the engine needs to know about one network the user wants to use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct NetworkSpec {
    /// Adapter name (see [`NetworkInterface::name`]).
    pub interface: String,
    /// Whether new connections may use it. A disabled network's running connections finish
    /// their current request and stop.
    pub enabled: bool,
    /// Speed limit of this network in bytes/s across all downloads (0 = unlimited).
    pub speed_limit: u64,
}

/// One network in use by the engine.
#[derive(Debug)]
pub struct Network {
    interface: String,
    /// Source address (Windows) / fallback when the adapter disappears from the list.
    local_addr: RwLock<Option<IpAddr>>,
    /// The adapter has an IPv4 / IPv6 address.
    families: RwLock<(bool, bool)>,
    enabled: AtomicBool,
    limiter: Arc<RateLimiter>,
    received: AtomicU64,
    connections: AtomicUsize,
    /// Failures to connect, per server (see [`server_of`]).
    reach: Mutex<HashMap<String, Reach>>,
}

impl Network {
    fn new(spec: &NetworkSpec, present: Option<&NetworkInterface>) -> Self {
        Self {
            interface: spec.interface.clone(),
            local_addr: RwLock::new(present.and_then(NetworkInterface::preferred_addr)),
            families: RwLock::new(present.map_or((false, false), NetworkInterface::families)),
            enabled: AtomicBool::new(spec.enabled),
            limiter: Arc::new(RateLimiter::new(spec.speed_limit)),
            received: AtomicU64::new(0),
            connections: AtomicUsize::new(0),
            reach: Mutex::default(),
        }
    }

    /// Adapter name.
    pub fn interface(&self) -> &str {
        &self.interface
    }

    /// The source address used for binding (see the module docs).
    pub fn local_addr(&self) -> Option<IpAddr> {
        *self.local_addr.read().expect("network lock")
    }

    /// Whether the adapter has an IPv4 / IPv6 address.
    pub fn families(&self) -> (bool, bool) {
        *self.families.read().expect("network lock")
    }

    fn set_present(&self, present: Option<&NetworkInterface>) {
        *self.local_addr.write().expect("network lock") =
            present.and_then(NetworkInterface::preferred_addr);
        *self.families.write().expect("network lock") =
            present.map_or((false, false), NetworkInterface::families);
    }

    /// Whether new connections may use this network.
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    /// This network's speed limiter (shared by every connection on it).
    pub fn limiter(&self) -> &Arc<RateLimiter> {
        &self.limiter
    }

    /// Bytes received over this network since the engine started.
    pub fn received(&self) -> u64 {
        self.received.load(Ordering::Relaxed)
    }

    pub(crate) fn add_received(&self, n: u64) {
        self.received.fetch_add(n, Ordering::Relaxed);
    }

    /// Connections currently assigned to this network.
    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::Relaxed)
    }

    /// Temporarily skipped for new connections to some server after failing to connect to
    /// it repeatedly (no internet on this network?).
    pub fn is_failing(&self) -> bool {
        let now = Instant::now();
        self.reach
            .lock()
            .expect("network lock")
            .values()
            .any(|r| r.benched_until.is_some_and(|t| now < t))
    }

    /// Temporarily skipped for new connections to `server` (see [`server_of`]).
    pub fn is_failing_for(&self, server: &str) -> bool {
        self.is_benched_for(server, Instant::now())
    }

    fn is_benched_for(&self, server: &str, now: Instant) -> bool {
        self.reach
            .lock()
            .expect("network lock")
            .get(server)
            .and_then(|r| r.benched_until)
            .is_some_and(|t| now < t)
    }

    /// A request to `server` over this network got an answer: it reaches the server.
    pub(crate) fn on_success(&self, server: &str) {
        let mut reach = self.reach.lock().expect("network lock");
        if !reach.is_empty() {
            reach.remove(server);
        }
    }

    /// A connection to `server` over this network could not be made. A server that accepts
    /// the connection and then resets it is not counted: that is the server's doing (a
    /// connection limit), not the network's. After a few failures in a row the network gets
    /// no new connections to that server for a while.
    pub(crate) fn on_connect_failure(&self, server: &str) {
        let now = Instant::now();
        let mut reach = self.reach.lock().expect("network lock");
        // Forget old failures, so the map stays small and occasional ones don't add up.
        reach.retain(|_, r| {
            now < r.last_failure + BENCH_FOR || r.benched_until.is_some_and(|t| now < t)
        });
        let r = reach.entry(server.to_string()).or_insert(Reach {
            failures: 0,
            last_failure: now,
            benched_until: None,
        });
        r.failures += 1;
        r.last_failure = now;
        if r.failures >= BENCH_AFTER_FAILURES {
            tracing::info!(network = %self.interface, server, "cannot reach the server, benched");
            r.benched_until = Some(now + BENCH_FOR);
        }
    }
}

/// A connection's claim on a network; releases the connection count when dropped.
#[derive(Debug)]
pub struct NetworkLease {
    network: Arc<Network>,
}

impl NetworkLease {
    /// The leased network.
    pub fn network(&self) -> &Arc<Network> {
        &self.network
    }
}

impl Drop for NetworkLease {
    fn drop(&mut self) {
        self.network.connections.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Live statistics of one network.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkStats {
    /// Adapter name.
    pub interface: String,
    /// Whether the adapter is currently present with an address.
    pub present: bool,
    /// Whether it is enabled.
    pub enabled: bool,
    /// Temporarily skipped after connection failures.
    pub failing: bool,
    /// Connections currently on it.
    pub connections: u32,
    /// Bytes received since the engine started.
    pub received: u64,
}

/// The networks the engine spreads connections over (empty = default route).
#[derive(Debug, Default)]
pub struct NetworkSet {
    networks: RwLock<Vec<Arc<Network>>>,
}

impl NetworkSet {
    /// Replaces the configuration. Networks that stay keep their counters and limiter (with
    /// the new rate); adapters that are not present right now are kept but get no connections
    /// until they reappear (see [`NetworkSet::refresh`]).
    pub fn configure(&self, specs: &[NetworkSpec]) {
        let present = interfaces(true);
        let mut g = self.networks.write().expect("networks lock");
        let mut next = Vec::with_capacity(specs.len());
        for spec in specs {
            if spec.interface.is_empty()
                || next
                    .iter()
                    .any(|n: &Arc<Network>| n.interface == spec.interface)
            {
                continue;
            }
            let iface = present.iter().find(|i| i.name == spec.interface);
            let net = match g.iter().find(|n| n.interface == spec.interface) {
                Some(n) => {
                    n.enabled.store(spec.enabled, Ordering::Release);
                    n.limiter.set_rate(spec.speed_limit);
                    n.set_present(iface);
                    n.clone()
                }
                None => Arc::new(Network::new(spec, iface)),
            };
            next.push(net);
        }
        // Connections still holding a removed network must see it switched off and move on.
        for n in g.iter().filter(|n| !next.iter().any(|m| Arc::ptr_eq(n, m))) {
            n.enabled.store(false, Ordering::Release);
        }
        *g = next;
    }

    /// Re-reads adapter addresses (after sleep, Wi-Fi changes, a phone being plugged in, …).
    pub fn refresh(&self) {
        let present = interfaces(true);
        for n in self.networks.read().expect("networks lock").iter() {
            n.set_present(present.iter().find(|i| i.name == n.interface));
        }
    }

    /// Whether more than the default route is configured (at least one enabled network).
    pub fn is_active(&self) -> bool {
        self.networks
            .read()
            .expect("networks lock")
            .iter()
            .any(|n| n.is_enabled())
    }

    /// Enables or disables one network immediately.
    pub fn set_enabled(&self, interface: &str, enabled: bool) {
        if let Some(n) = self.get(interface) {
            n.enabled.store(enabled, Ordering::Release);
        }
    }

    /// Changes one network's speed limit immediately.
    pub fn set_speed_limit(&self, interface: &str, bytes_per_sec: u64) {
        if let Some(n) = self.get(interface) {
            n.limiter.set_rate(bytes_per_sec);
        }
    }

    /// A configured network by adapter name.
    pub fn get(&self, interface: &str) -> Option<Arc<Network>> {
        self.networks
            .read()
            .expect("networks lock")
            .iter()
            .find(|n| n.interface == interface)
            .cloned()
    }

    /// Total bytes received over configured networks since the engine started.
    pub fn received(&self) -> u64 {
        self.networks
            .read()
            .expect("networks lock")
            .iter()
            .map(|n| n.received())
            .sum()
    }

    /// Live statistics of every configured network.
    pub fn stats(&self) -> Vec<NetworkStats> {
        self.networks
            .read()
            .expect("networks lock")
            .iter()
            .map(|n| NetworkStats {
                interface: n.interface.clone(),
                present: n.local_addr().is_some(),
                enabled: n.is_enabled(),
                failing: n.is_failing(),
                connections: n.connections() as u32,
                received: n.received(),
            })
            .collect()
    }

    /// Whether an enabled network has a speed limit (tails are not raced then: duplicate
    /// bytes would eat the capped budget).
    pub fn any_capped(&self) -> bool {
        self.networks
            .read()
            .expect("networks lock")
            .iter()
            .any(|n| n.is_enabled() && n.limiter.rate() != 0)
    }

    /// Picks the network for a new connection to `server`: the usable network with the
    /// fewest connections (ties go to the earlier one in the user's order). Falls back to
    /// benched networks when every enabled one fails for that server. `None` = use the
    /// default route.
    pub fn lease(&self, server: &str) -> Option<NetworkLease> {
        let now = Instant::now();
        let g = self.networks.read().expect("networks lock");
        let usable = |n: &&Arc<Network>| n.is_enabled() && n.local_addr().is_some();
        let pick = g
            .iter()
            .filter(usable)
            .filter(|n| !n.is_benched_for(server, now))
            .min_by_key(|n| n.connections())
            .or_else(|| g.iter().filter(usable).min_by_key(|n| n.connections()))?
            .clone();
        pick.connections.fetch_add(1, Ordering::AcqRel);
        Some(NetworkLease { network: pick })
    }
}

/// DNS resolver that keeps only addresses of the families a network can reach: an adapter
/// without an IPv6 address (common for Ethernet next to an IPv6 phone tether) cannot connect
/// to a server's IPv6 address, and Windows binds an IPv4 source that can't reach IPv6 either.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FamilyResolver {
    pub v4: bool,
    pub v6: bool,
}

impl reqwest::dns::Resolve for FamilyResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let FamilyResolver { v4, v6 } = *self;
        Box::pin(async move {
            let host = format!("{}:0", name.as_str());
            let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host(host)
                .await?
                .filter(|a| if a.is_ipv4() { v4 } else { v6 })
                .collect();
            if addrs.is_empty() {
                return Err(format!(
                    "{} has no {} address for this network",
                    name.as_str(),
                    if v4 { "IPv4" } else { "IPv6" }
                )
                .into());
            }
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: &str = "files.example:443";

    #[test]
    fn families_follow_the_adapter_addresses() {
        let i = |a: &[&str]| NetworkInterface {
            name: "eth0".into(),
            addrs: a.iter().map(|s| s.parse().unwrap()).collect(),
        };
        assert_eq!(i(&["192.168.1.3"]).families(), (true, false));
        assert_eq!(i(&["10.0.0.2", "2409:40c4::1"]).families(), (true, true));
        assert_eq!(i(&["2409:40c4::1"]).families(), (false, true));
    }

    #[tokio::test]
    async fn family_resolver_drops_unreachable_families() {
        use reqwest::dns::Resolve;
        let name: reqwest::dns::Name = "localhost".parse().unwrap();
        let only_v4: Vec<_> = FamilyResolver {
            v4: true,
            v6: false,
        }
        .resolve(name)
        .await
        .unwrap()
        .collect();
        assert!(!only_v4.is_empty() && only_v4.iter().all(|a| a.is_ipv4()));
    }

    fn spec(name: &str) -> NetworkSpec {
        NetworkSpec {
            interface: name.into(),
            enabled: true,
            speed_limit: 0,
        }
    }

    fn set_with(names: &[&str]) -> NetworkSet {
        let set = NetworkSet::default();
        set.configure(&names.iter().map(|n| spec(n)).collect::<Vec<_>>());
        // Tests do not depend on the machine's adapters: give every network an address.
        for n in set.networks.read().unwrap().iter() {
            *n.local_addr.write().unwrap() = Some("192.0.2.1".parse().unwrap());
        }
        set
    }

    #[test]
    fn empty_set_uses_default_route() {
        let set = NetworkSet::default();
        assert!(!set.is_active());
        assert!(set.lease(S).is_none());
    }

    #[test]
    fn leases_spread_evenly() {
        let set = set_with(&["a", "b"]);
        let l: Vec<_> = (0..4).map(|_| set.lease(S).unwrap()).collect();
        let on_a = l.iter().filter(|l| l.network().interface() == "a").count();
        assert_eq!(on_a, 2);
        drop(l);
        assert_eq!(set.get("a").unwrap().connections(), 0);
    }

    #[test]
    fn disabled_and_failing_networks_are_skipped() {
        let set = set_with(&["a", "b"]);
        set.set_enabled("a", false);
        for _ in 0..3 {
            assert_eq!(set.lease(S).unwrap().network().interface(), "b");
        }
        set.set_enabled("a", true);
        let a = set.get("a").unwrap();
        for _ in 0..BENCH_AFTER_FAILURES {
            a.on_connect_failure(S);
        }
        let _hold = set.lease(S).unwrap();
        assert_eq!(set.lease(S).unwrap().network().interface(), "b");
        a.on_success(S);
        // With "b" holding more connections now, the recovered "a" is picked again.
        let _b = set.lease(S).unwrap();
        assert_eq!(set.lease(S).unwrap().network().interface(), "a");
    }

    #[test]
    fn all_failing_falls_back_instead_of_stalling() {
        let set = set_with(&["a"]);
        let a = set.get("a").unwrap();
        for _ in 0..BENCH_AFTER_FAILURES {
            a.on_connect_failure(S);
        }
        assert!(set.lease(S).is_some());
    }

    #[test]
    fn reconfigure_keeps_counters() {
        let set = set_with(&["a"]);
        set.get("a").unwrap().add_received(10);
        set.configure(&[
            NetworkSpec {
                speed_limit: 5,
                ..spec("a")
            },
            spec("b"),
        ]);
        let a = set.get("a").unwrap();
        assert_eq!(a.received(), 10);
        assert_eq!(a.limiter().rate(), 5);
        assert_eq!(set.stats().len(), 2);
    }

    #[test]
    fn a_network_is_benched_only_for_the_server_it_cannot_reach() {
        let set = set_with(&["a", "b"]);
        let a = set.get("a").unwrap();
        for _ in 0..BENCH_AFTER_FAILURES {
            a.on_connect_failure("blocked.example:443");
        }
        assert!(a.is_failing_for("blocked.example:443") && a.is_failing());
        assert!(!a.is_failing_for(S), "other servers still go over it");
        assert_eq!(set.lease(S).unwrap().network().interface(), "a");
        assert_eq!(
            set.lease("blocked.example:443")
                .unwrap()
                .network()
                .interface(),
            "b"
        );
    }

    #[test]
    fn occasional_failures_do_not_add_up_to_a_bench() {
        let set = set_with(&["a"]);
        let a = set.get("a").unwrap();
        let Some(long_ago) = Instant::now().checked_sub(BENCH_FOR + Duration::from_secs(1)) else {
            return;
        };
        a.reach.lock().unwrap().insert(
            S.to_string(),
            Reach {
                failures: BENCH_AFTER_FAILURES - 1,
                last_failure: long_ago,
                benched_until: None,
            },
        );
        a.on_connect_failure(S);
        assert!(
            !a.is_failing_for(S),
            "failures long apart are not 'in a row'"
        );
        a.on_connect_failure(S);
        a.on_connect_failure(S);
        assert!(a.is_failing_for(S));
    }

    #[test]
    fn a_capped_network_stops_tail_racing() {
        let set = set_with(&["a", "b"]);
        assert!(!set.any_capped());
        set.set_speed_limit("b", 1000);
        assert!(set.any_capped());
        set.set_enabled("b", false);
        assert!(
            !set.any_capped(),
            "a switched-off network gets no connections"
        );
    }

    #[test]
    fn servers_are_told_apart_by_host_and_port() {
        let u = |s: &str| url::Url::parse(s).unwrap();
        assert_eq!(server_of(&u("https://a.example/x")), "a.example:443");
        assert_eq!(server_of(&u("http://a.example/x")), "a.example:80");
        assert_eq!(server_of(&u("http://a.example:8080/")), "a.example:8080");
    }

    #[test]
    fn removed_networks_are_switched_off_for_their_connections() {
        let set = set_with(&["a", "b"]);
        let held = set.lease(S).unwrap();
        assert_eq!(held.network().interface(), "a");
        set.configure(&[spec("b")]);
        assert!(
            !held.network().is_enabled(),
            "a connection on a removed network moves on"
        );
        set.configure(&[]);
        assert!(!set.get("b").is_some_and(|b| b.is_enabled()) && !set.is_active());
    }

    #[test]
    fn link_local_and_loopback_are_not_usable() {
        assert!(!usable_addr(&"127.0.0.1".parse().unwrap()));
        assert!(!usable_addr(&"169.254.1.1".parse().unwrap()));
        assert!(!usable_addr(&"fe80::1".parse().unwrap()));
        assert!(usable_addr(&"192.168.1.5".parse().unwrap()));
        assert!(usable_addr(&"2001:db8::1".parse().unwrap()));
    }
}
