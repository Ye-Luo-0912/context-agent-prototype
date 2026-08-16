pub(crate) mod full;
pub(crate) mod minor;
pub(crate) mod reachability;

use serde::{Deserialize, Serialize};

/// Resume points for bounded GC work. Full passes with `gc_work_batch`
/// covering the whole heap/store still visit every item in stable order.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub(crate) struct GcWorkCursor {
    #[serde(default)]
    pub heap: usize,
    #[serde(default)]
    pub warm: usize,
    #[serde(default)]
    pub stored: usize,
}
