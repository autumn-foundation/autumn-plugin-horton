//! The `horton` health indicator.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use autumn_web::actuator::{HealthCheckOutput, HealthIndicator};
use serde_json::json;

use crate::slot::Slot;

/// Reports `UP` while the store thread runs, and `DOWN` when it is not
/// open or has stopped. It reads the last stats and does no I/O.
pub struct HortonHealth {
    slot: Slot,
}

impl HortonHealth {
    pub(crate) const fn new(slot: Slot) -> Self {
        Self { slot }
    }
}

impl HealthIndicator for HortonHealth {
    fn check(&self) -> Pin<Box<dyn Future<Output = HealthCheckOutput> + Send + '_>> {
        let output = match self.slot.get() {
            None => {
                HealthCheckOutput::down().with_details(details([("reason", json!("not open"))]))
            }
            Some(db) if db.is_closed() => {
                HealthCheckOutput::down().with_details(details([("reason", json!("closed"))]))
            }
            Some(db) => db.snapshot().map_or_else(HealthCheckOutput::up, |s| {
                HealthCheckOutput::up().with_details(details([
                    ("device_blocks", json!(s.device_blocks)),
                    ("slots_used", json!(s.slots_used)),
                    ("slots_free", json!(s.slots_free)),
                    ("level_tables", json!(s.level_tables)),
                    ("compaction_pending", json!(s.compaction_pending)),
                ]))
            }),
        };
        Box::pin(std::future::ready(output))
    }
}

fn details<const N: usize>(
    pairs: [(&str, serde_json::Value); N],
) -> HashMap<String, serde_json::Value> {
    pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect()
}

#[cfg(test)]
mod tests {
    use autumn_web::actuator::HealthStatus;

    use super::*;

    #[tokio::test]
    async fn a_store_that_is_not_open_is_down() {
        let out = HortonHealth::new(Slot::default()).check().await;
        assert_eq!(out.status, HealthStatus::Down);
        assert_eq!(out.details["reason"], "not open");
    }
}
