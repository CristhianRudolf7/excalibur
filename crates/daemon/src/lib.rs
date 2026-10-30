//! Shared daemon hosting: separate runtimes, bounded CPU pool and reserved model lanes.
pub mod lanes;
pub mod runtimes;
pub use lanes::*;
pub use runtimes::*;

pub mod resources;
pub use resources::*;
pub mod actions;
pub use actions::*;
pub mod host;
pub use host::*;
pub mod command;

pub mod worker_tools;
pub use worker_tools::*;
pub mod task_gate;

pub mod planning_tools;
pub use planning_tools::ScopedPlanning;

pub mod projection;

mod project_scope;
