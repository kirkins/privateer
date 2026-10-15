//! An optional, read-only operator view. It never calls job reconciliation,
//! consumes notifications, or owns an agent process.
pub mod boat;
pub mod captain;
pub mod fleet;
pub mod herdr;
pub mod model;
pub mod ui;

pub mod command;
pub mod mailbox;
