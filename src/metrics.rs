//! The `horton_*` Prometheus families.

use autumn_web::actuator::{MetricFamily, MetricKind, MetricSample, MetricsSource};

use crate::handle::Stats;
use crate::slot::Slot;

/// Exports the store counters. It reads the last stats and does no I/O.
pub struct HortonMetrics {
    slot: Slot,
}

impl HortonMetrics {
    pub(crate) const fn new(slot: Slot) -> Self {
        Self { slot }
    }
}

#[allow(
    clippy::cast_precision_loss,
    reason = "Prometheus samples are f64; counters above 2^53 lose only low bits"
)]
const fn f(v: u64) -> f64 {
    v as f64
}

fn family(name: &str, help: &str, kind: MetricKind, samples: Vec<MetricSample>) -> MetricFamily {
    MetricFamily {
        name: name.to_owned(),
        help: help.to_owned(),
        kind,
        samples,
    }
}

fn one(value: f64) -> Vec<MetricSample> {
    vec![MetricSample {
        labels: Vec::new(),
        value,
    }]
}

fn labelled(label: &str, values: impl IntoIterator<Item = (String, f64)>) -> Vec<MetricSample> {
    values
        .into_iter()
        .map(|(l, value)| MetricSample {
            labels: vec![(label.to_owned(), l)],
            value,
        })
        .collect()
}

fn counters(s: &Stats) -> Vec<MetricFamily> {
    let counter =
        |name: &str, help: &str, v: u64| family(name, help, MetricKind::Counter, one(f(v)));
    vec![
        counter(
            "horton_io_reads_total",
            "Block reads from the store file.",
            s.io.reads,
        ),
        counter(
            "horton_io_writes_total",
            "Block writes to the store file.",
            s.io.writes,
        ),
        counter(
            "horton_io_syncs_total",
            "fdatasync calls on the store file.",
            s.io.syncs,
        ),
        counter("horton_write_groups_total", "Group commits.", s.groups),
        counter(
            "horton_writes_total",
            "Caller writes committed.",
            s.grouped_writes,
        ),
        counter("horton_flushes_total", "Memtable flushes.", s.flushes),
        counter(
            "horton_compaction_jobs_total",
            "Compaction jobs done.",
            s.compaction_jobs,
        ),
        counter(
            "horton_compaction_steps_total",
            "Compaction steps done.",
            s.compaction_steps,
        ),
        counter(
            "horton_cache_hits_total",
            "Block reads served by the cache.",
            s.cache_hits,
        ),
        counter(
            "horton_cache_misses_total",
            "Block reads that missed the cache.",
            s.cache_misses,
        ),
        family(
            "horton_device_blocks",
            "Store file size in 16 KiB blocks.",
            MetricKind::Gauge,
            one(f(s.device_blocks)),
        ),
        family(
            "horton_slots",
            "Table slots by state.",
            MetricKind::Gauge,
            labelled(
                "state",
                [
                    ("used".to_owned(), f(u64::from(s.slots_used))),
                    ("free".to_owned(), f(u64::from(s.slots_free))),
                ],
            ),
        ),
        family(
            "horton_level_tables",
            "Tables per LSM level.",
            MetricKind::Gauge,
            labelled(
                "level",
                s.level_tables
                    .iter()
                    .enumerate()
                    .map(|(l, n)| (l.to_string(), f(*n as u64))),
            ),
        ),
    ]
}

impl MetricsSource for HortonMetrics {
    fn collect(&self) -> Vec<MetricFamily> {
        let db = self.slot.get();
        let up = db.is_some_and(|db| !db.is_closed());
        let mut out = vec![family(
            "horton_up",
            "1 while the store thread runs.",
            MetricKind::Gauge,
            one(if up { 1.0 } else { 0.0 }),
        )];
        if let Some(stats) = db.and_then(crate::handle::Horton::snapshot) {
            out.extend(counters(&stats));
        }
        out
    }
}
