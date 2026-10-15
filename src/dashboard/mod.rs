//! Optional fleet observations and a command-backed captain mailbox.
//! Task observation never reconciles jobs or consumes task notifications;
//! the dashboard does not own agent processes.
pub mod boat;
pub mod captain;
pub mod fleet;
pub mod herdr;
pub mod model;
pub mod ui;

pub mod command;
pub mod mailbox;
