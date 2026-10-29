//! Shared noninteractive execution and reproducible evaluation contracts.
pub mod connection;
pub mod profile;
mod runner;
pub mod skills;
mod tools;
pub use runner::*;
pub use tools::ExecutionTools;

pub fn new_episode_id() -> String {
    excalibur_types::RunId::new().to_string()
}
