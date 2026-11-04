//! Dormant, authority-free qualification. This is NOT deployment admission,
//! a launch token, or adapter execution. Only an explicitly trusted isolated
//! delegation may be supplied; never discover or modify the live host subtree.
use std::path::Path;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub(crate) use linux::{Qualified, SealedExecutable, SealedInput};

#[derive(Debug)]
pub(crate) struct Refusal {
    pub(crate) reason: String,
    // If cleanup is inconclusive, this identifies custody that MUST be recorded
    // and inspected. No caller may release capacity merely on this error.
    pub(crate) retained: Option<serde_json::Value>,
}

#[cfg(not(target_os = "linux"))]
#[derive(Debug)]
pub(crate) struct Qualified;

pub(crate) fn qualify(delegation: &Path) -> Result<Qualified, Refusal> {
    #[cfg(target_os = "linux")]
    return linux::qualify(delegation);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = delegation;
        Err(Refusal {
            reason: "deployment containment requires qualified Linux cgroup v2".into(),
            retained: None,
        })
    }
}

#[cfg(test)]
mod tests;
