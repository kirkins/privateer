//! Optional operator fleet view: read-only state snapshots, command-backed
//! captain mail and management, and attended boarding. Observation never
//! reconciles jobs or consumes task notifications; the dashboard owns no agents.
pub mod boat;
pub mod bridge;
pub mod captain;
pub mod fleet;
pub use crate::herdr;
pub mod model;
pub mod overview;
pub mod ui;
pub mod voyage;

pub mod command;
pub mod mailbox;

pub mod forms;
