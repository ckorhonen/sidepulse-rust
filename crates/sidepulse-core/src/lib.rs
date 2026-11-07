//! Native SidePulse components. Phase 1 ports the agent hook (`sidepulse-hook`);
//! the modules are shared with the status-bar core in later phases.

pub mod audit;
pub mod collector;
pub mod hook;
pub mod ipc;
pub mod logrot;
pub mod models;
pub mod origin;
pub mod providers;
pub mod pyjson;
pub mod battery;
pub mod devices;
pub mod lid;
pub mod monitor;
pub mod power;
