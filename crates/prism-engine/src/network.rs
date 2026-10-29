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

use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::limiter::RateLimiter;

/// Consecutive connection failures after which a network is taken out of rotation.
const BENCH_AFTER_FAILURES: u32 = 3;
/// How long a failing network is not given new connections.
const BENCH_FOR: Duration = Duration::from_secs(30);
/// A network that received data this recently works: its failed connections are the server's
/// doing (a connection limit, a reset), not the network's.
const WORKING_WITHIN: Duration = Duration::from_secs(3);

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
    failures: AtomicU32,
    benched_until: RwLock<Option<Instant>>,
    /// When data last arrived, in ms since `created` (0 = never).
    created: Instant,
    last_received_ms: AtomicU64,
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
            failures: AtomicU32::new(0),
            benched_until: RwLock::new(None),
            created: Instant::now(),
            last_received_ms: AtomicU64::new(0),
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
        let ms = self.created.elapsed().as_millis() as u64;
        self.last_received_ms.store(ms.max(1), Ordering::Relaxed);
    }

    fn received_within(&self, d: Duration, now: Instant) -> bool {
        match self.last_received_ms.load(Ordering::Relaxed) {
            0 => false,
            ms => now.saturating_duration_since(self.created) <= Duration::from_millis(ms) + d,
        }
    }

    /// Connections currently assigned to this network.
    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::Relaxed)
    }

    /// Temporarily skipped for new connections after repeated connection failures.
    pub fn is_failing(&self) -> bool {
        self.is_benched(Instant::now())
    }

    fn is_benched(&self, now: Instant) -> bool {
        self.benched_until
            .read()
            .expect("network lock")
            .is_some_and(|t| now < t)
    }

    /// A connection on this network received data: it works.
    pub(crate) fn on_success(&self) {
        if self.failures.swap(0, Ordering::AcqRel) > 0 {
            *self.benched_until.write().expect("network lock") = None;
        }
    }

    /// A connection on this network could not connect. After a few failures in a row while
    /// nothing arrives over it, the network gets no new connections for a while.
    pub(crate) fn on_failure(&self) {
        if self.received_within(WORKING_WITHIN, Instant::now()) {
            return;
        }
        let n = self.failures.fetch_add(1, Ordering::AcqRel) + 1;
        if n >= BENCH_AFTER_FAILURES {
            tracing::info!(network = %self.interface, "network failing, benched");
            *self.benched_until.write().expect("network lock") = Some(Instant::now() + BENCH_FOR);
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
        let now = Instant::now();
        self.networks
            .read()
            .expect("networks lock")
            .iter()
            .map(|n| NetworkStats {
                interface: n.interface.clone(),
                present: n.local_addr().is_some(),
                enabled: n.is_enabled(),
                failing: n.is_benched(now),
                connections: n.connections() as u32,
                received: n.received(),
            })
            .collect()
    }

    /// Picks the network for a new connection: the usable network with the fewest
    /// connections (ties go to the earlier one in the user's order). Falls back to benched
    /// networks when every enabled one is failing. `None` = use the default route.
    pub fn lease(&self) -> Option<NetworkLease> {
        let now = Instant::now();
        let g = self.networks.read().expect("networks lock");
        let usable = |n: &&Arc<Network>| n.is_enabled() && n.local_addr().is_some();
        let pick = g
            .iter()
            .filter(usable)
            .filter(|n| !n.is_benched(now))
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
        assert!(set.lease().is_none());
    }

    #[test]
    fn leases_spread_evenly() {
        let set = set_with(&["a", "b"]);
        let l: Vec<_> = (0..4).map(|_| set.lease().unwrap()).collect();
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
            assert_eq!(set.lease().unwrap().network().interface(), "b");
        }
        set.set_enabled("a", true);
        let a = set.get("a").unwrap();
        for _ in 0..BENCH_AFTER_FAILURES {
            a.on_failure();
        }
        let _hold = set.lease().unwrap();
        assert_eq!(set.lease().unwrap().network().interface(), "b");
        a.on_success();
        // With "b" holding more connections now, the recovered "a" is picked again.
        let _b = set.lease().unwrap();
        assert_eq!(set.lease().unwrap().network().interface(), "a");
    }

    #[test]
    fn all_failing_falls_back_instead_of_stalling() {
        let set = set_with(&["a"]);
        let a = set.get("a").unwrap();
        for _ in 0..BENCH_AFTER_FAILURES {
            a.on_failure();
        }
        assert!(set.lease().is_some());
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
    fn a_network_that_delivers_data_is_not_benched_for_refused_connections() {
        let set = set_with(&["a"]);
        let a = set.get("a").unwrap();
        a.add_received(1);
        for _ in 0..2 * BENCH_AFTER_FAILURES {
            a.on_failure();
        }
        assert!(
            !a.is_failing(),
            "a server refusing extra connections is not a dead network"
        );
        // Once nothing has arrived for a while, failures count again.
        let later = Instant::now() + WORKING_WITHIN + Duration::from_secs(1);
        assert!(!a.received_within(WORKING_WITHIN, later));
    }

    #[test]
    fn removed_networks_are_switched_off_for_their_connections() {
        let set = set_with(&["a", "b"]);
        let held = set.lease().unwrap();
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
