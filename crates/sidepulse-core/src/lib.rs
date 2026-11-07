//! Native SidePulse components. Phase 1 ports the agent hook (`sidepulse-hook`);
//! the modules are shared with the status-bar core in later phases.

pub mod audit;
pub mod battery;
pub mod collector;
pub mod devices;
pub mod hook;
pub mod ipc;
pub mod lid;
pub mod logrot;
pub mod models;
pub mod monitor;
pub mod origin;
pub mod power;
pub mod providers;
pub mod pyjson;
