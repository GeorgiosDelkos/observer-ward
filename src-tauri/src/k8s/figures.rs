//! Node disk, network and PVC figures for one poll, whichever source they
//! come from: kubelet stats (which need `nodes/proxy`) or a Prometheus
//! server in the cluster. Both fill a [`HostFigures`], so the node and pod
//! reports do not depend on the source.

use std::collections::HashMap;

use crate::metrics::{Capacity, NetRate, NetSample};

/// What one poll knows about disks, networks and PVCs. Empty when neither
/// source is available.
#[derive(Debug, Default)]
pub(super) struct HostFigures {
    /// Root filesystem per node, keyed by node name.
    pub(super) node_disks: HashMap<String, Capacity>,
    /// Throughput of all nodes together. `None` until a source has a rate:
    /// kubelet counters need two polls.
    pub(super) cluster_net: Option<NetRate>,
    /// Keyed by namespace; a namespace without figures is absent.
    pub(super) namespaces: HashMap<String, NamespaceFigures>,
}

impl HostFigures {
    /// The root filesystems of every node together.
    pub(super) fn cluster_disk(&self) -> Capacity {
        self.node_disks
            .values()
            .fold(Capacity::default(), |total, disk| Capacity {
                used_bytes: total.used_bytes.saturating_add(disk.used_bytes),
                capacity_bytes: total.capacity_bytes.saturating_add(disk.capacity_bytes),
            })
    }
}

/// One namespace's pod figures.
#[derive(Debug, Default)]
pub(super) struct NamespaceFigures {
    /// Throughput per pod name.
    pub(super) pod_net: HashMap<String, NetRate>,
    pub(super) pvc: PvcUsage,
}

/// PVC usage as the source reports it. Kubelet stats list volumes per pod;
/// Prometheus reports per claim, which the pod specs map to pods.
#[derive(Debug)]
pub(super) enum PvcUsage {
    /// Summed over each pod's PVC-backed volumes, keyed by pod name.
    ByPod(HashMap<String, Capacity>),
    /// Keyed by claim name.
    ByClaim(HashMap<String, Capacity>),
}

impl Default for PvcUsage {
    fn default() -> Self {
        Self::ByPod(HashMap::new())
    }
}

/// Throughput per key between two sets of cumulative counters. A key
/// missing from `prev` has no rate; one whose counters went backwards (a
/// restart) reads 0 for that poll, as `NetSample::rate_since` gives.
pub(super) fn rates_since(
    now: &HashMap<String, NetSample>,
    prev: &HashMap<String, NetSample>,
) -> HashMap<String, NetRate> {
    now.iter()
        .filter_map(|(key, sample)| {
            let rate = sample.rate_since(prev.get(key)?)?;
            Some((key.clone(), rate))
        })
        .collect()
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "panicking on failure is standard in tests"
)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    fn sample(rx_bytes: u64, tx_bytes: u64, at: Instant) -> NetSample {
        NetSample {
            rx_bytes,
            tx_bytes,
            at,
        }
    }

    #[test]
    fn rates_need_a_previous_sample_per_key() {
        let start = Instant::now();
        let later = start + Duration::from_secs(1);
        let prev = HashMap::from([("web".to_string(), sample(0, 0, start))]);
        let now = HashMap::from([
            ("web".to_string(), sample(2_000, 1_000, later)),
            ("new".to_string(), sample(5_000, 5_000, later)),
        ]);

        let rates = rates_since(&now, &prev);

        let web = rates.get("web").expect("two samples");
        assert_eq!((web.rx_bytes_per_sec, web.tx_bytes_per_sec), (2_000, 1_000));
        assert!(!rates.contains_key("new"), "one sample has no rate");
    }

    #[test]
    fn cluster_disk_sums_nodes() {
        let figures = HostFigures {
            node_disks: HashMap::from([
                (
                    "a".to_string(),
                    Capacity {
                        used_bytes: 1,
                        capacity_bytes: 10,
                    },
                ),
                (
                    "b".to_string(),
                    Capacity {
                        used_bytes: 2,
                        capacity_bytes: 20,
                    },
                ),
            ]),
            ..HostFigures::default()
        };

        assert_eq!(
            figures.cluster_disk(),
            Capacity {
                used_bytes: 3,
                capacity_bytes: 30,
            }
        );
    }
}
