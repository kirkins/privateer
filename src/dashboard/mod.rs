//! Optional operator fleet view: read-only state snapshots, with explicit
//! mailbox/management commands and attended boarding. It never owns agents.
pub mod boat;
pub mod captain;
pub mod fleet;
pub use crate::herdr;
pub mod model;
pub mod ui;

pub mod command;
pub mod mailbox;

pub mod forms;
