//! Multi-network settings → engine configuration, and data-allowance bookkeeping.

use chrono::{Datelike, Days, Months, NaiveDate};
use prism_engine::{NetworkInterface, NetworkSpec};
use serde::{Deserialize, Serialize};

use crate::settings::{AllowancePeriod, Networks};

/// The engine's network list for these settings and the adapters present right now.
/// Configured adapters that are missing stay in the list (they come back after sleep or when
/// a phone is plugged in again); present adapters without an entry are used when
/// [`used_by_default`].
pub fn engine_specs(settings: &Networks, present: &[NetworkInterface]) -> Vec<NetworkSpec> {
    if !settings.enabled {
        return Vec::new();
    }
    let mut out: Vec<NetworkSpec> = settings
        .entries
        .iter()
        .filter(|e| !e.interface.is_empty())
        .map(|e| NetworkSpec {
            interface: e.interface.clone(),
            enabled: e.enabled,
            speed_limit: e.speed_limit,
        })
        .collect();
    for i in present {
        if settings.entry(&i.name).is_none() && used_by_default(&i.name) {
            out.push(NetworkSpec {
                interface: i.name.clone(),
                enabled: true,
                speed_limit: 0,
            });
        }
    }
    out
}

/// Whether an adapter without an entry is used. Virtual adapters (containers, VMs, VPN
/// tunnels) usually have no internet path of their own, so they stay off until switched on.
pub fn used_by_default(interface: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "docker",
        "br-",
        "virbr",
        "veth",
        "vnet",
        "vmnet",
        "vboxnet",
        "lxc",
        "lxd",
        "cni",
        "flannel",
        "cali",
        "podman",
        "tun",
        "tap",
        "wg",
        "utun",
        "tailscale",
        "zt",
        "ipsec",
        "awdl",
        "llw",
        "bridge",
    ];
    // Windows lists adapters by their friendly name. Hyper-V's internal switches are
    // virtual, but an external switch ("vEthernet (External Switch)") is the computer's real
    // uplink: the physical adapter under it has no address of its own.
    const CONTAINS: &[&str] = &[
        "vethernet (default switch)",
        "vethernet (nat)",
        "virtualbox",
        "vmware",
        "hyper-v",
        "wsl",
        "loopback",
        "teredo",
        "isatap",
        "tailscale",
        "zerotier",
        "wireguard",
        "openvpn",
        "tap-windows",
        "npcap",
    ];
    let name = interface.to_ascii_lowercase();
    // Unix adapter names have no spaces ("veth1a2b"); Windows friendly names may ("vEthernet
    // (External Switch)" must not count as a veth pair).
    let unix_virtual = !name.contains(' ') && PREFIXES.iter().any(|p| name.starts_with(p));
    !unix_virtual && !CONTAINS.iter().any(|c| name.contains(c))
}

/// First day of the allowance period containing `date` (weeks start on Monday).
pub fn period_start(period: AllowancePeriod, date: NaiveDate) -> NaiveDate {
    match period {
        AllowancePeriod::Daily => date,
        AllowancePeriod::Weekly => {
            date - Days::new(u64::from(date.weekday().num_days_from_monday()))
        }
        AllowancePeriod::Monthly => date.with_day(1).expect("day 1 exists"),
    }
}

/// First day of the next period (when the allowance resets).
pub fn next_period_start(period: AllowancePeriod, date: NaiveDate) -> NaiveDate {
    let start = period_start(period, date);
    match period {
        AllowancePeriod::Daily => start + Days::new(1),
        AllowancePeriod::Weekly => start + Days::new(7),
        AllowancePeriod::Monthly => start + Months::new(1),
    }
}

/// Bytes downloaded in the current allowance period (persisted).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DataUsage {
    pub period_start: NaiveDate,
    pub used: u64,
}

impl DataUsage {
    pub fn new(period: AllowancePeriod, today: NaiveDate) -> Self {
        Self {
            period_start: period_start(period, today),
            used: 0,
        }
    }

    /// Starts a new period when `today` is past the current one (or the period kind changed).
    /// Returns whether it reset.
    pub fn roll(&mut self, period: AllowancePeriod, today: NaiveDate) -> bool {
        let start = period_start(period, today);
        if start != self.period_start {
            *self = Self {
                period_start: start,
                used: 0,
            };
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::NetworkEntry;

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    fn iface(name: &str) -> NetworkInterface {
        NetworkInterface {
            name: name.into(),
            addrs: vec!["192.0.2.1".parse().unwrap()],
        }
    }

    #[test]
    fn periods() {
        // 2026-10-25 is a Sunday.
        let day = d("2026-10-25");
        assert_eq!(period_start(AllowancePeriod::Daily, day), day);
        assert_eq!(period_start(AllowancePeriod::Weekly, day), d("2026-10-19"));
        assert_eq!(period_start(AllowancePeriod::Monthly, day), d("2026-10-01"));
        assert_eq!(
            next_period_start(AllowancePeriod::Weekly, day),
            d("2026-10-26")
        );
        assert_eq!(
            next_period_start(AllowancePeriod::Monthly, d("2026-12-31")),
            d("2027-01-01")
        );
    }

    #[test]
    fn usage_rolls_over() {
        let mut u = DataUsage::new(AllowancePeriod::Monthly, d("2026-10-25"));
        u.used = 10;
        assert!(!u.roll(AllowancePeriod::Monthly, d("2026-10-31")));
        assert_eq!(u.used, 10);
        assert!(u.roll(AllowancePeriod::Monthly, d("2026-11-01")));
        assert_eq!(u.used, 0);
        assert_eq!(u.period_start, d("2026-11-01"));
    }

    #[test]
    fn specs_follow_settings() {
        let present = [iface("wlan0"), iface("eth0")];
        let mut s = Networks::default();
        assert!(engine_specs(&s, &present).is_empty(), "off by default");
        s.enabled = true;
        s.entries.push(NetworkEntry {
            interface: "eth0".into(),
            enabled: false,
            speed_limit: 7,
            ..NetworkEntry::default()
        });
        s.entries.push(NetworkEntry {
            interface: "usb0".into(),
            ..NetworkEntry::default()
        });
        let specs = engine_specs(&s, &present);
        let names: Vec<_> = specs.iter().map(|s| s.interface.as_str()).collect();
        assert_eq!(names, ["eth0", "usb0", "wlan0"]);
        assert!(!specs[0].enabled);
        assert_eq!(specs[0].speed_limit, 7);
        assert!(specs[2].enabled);
    }

    #[test]
    fn virtual_adapters_are_off_until_switched_on() {
        for name in [
            "docker0",
            "br-1a2b",
            "virbr0",
            "tun0",
            "wg0",
            "utun3",
            "vEthernet (WSL)",
            "vEthernet (Default Switch)",
        ] {
            assert!(!used_by_default(name), "{name}");
        }
        for name in [
            "wlan0",
            "eth0",
            "enp3s0",
            "en0",
            "usb0",
            "rndis0",
            "Wi-Fi",
            "Ethernet 2",
            "vEthernet (External Switch)",
        ] {
            assert!(used_by_default(name), "{name}");
        }
        let mut s = Networks {
            enabled: true,
            ..Networks::default()
        };
        let present = [iface("eth0"), iface("docker0")];
        let names = |s: &Networks| -> Vec<String> {
            engine_specs(s, &present)
                .into_iter()
                .map(|n| n.interface)
                .collect()
        };
        assert_eq!(names(&s), ["eth0"]);
        s.entries.push(NetworkEntry {
            interface: "docker0".into(),
            ..NetworkEntry::default()
        });
        assert_eq!(names(&s), ["docker0", "eth0"], "switched on by the user");
    }
}
