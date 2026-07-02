mod anthropic;
mod auth;
mod chat;
mod cli;
mod config;
mod cookies;
mod errors;
mod local_config;
mod logging;
mod models;
mod responses;
mod server;
mod service_tier;
mod tray;

pub use cli::run;

#[cfg(test)]
pub(crate) use anthropic::*;
#[cfg(test)]
pub(crate) use auth::*;
#[cfg(test)]
pub(crate) use chat::*;
#[cfg(test)]
pub(crate) use config::*;
#[cfg(test)]
pub(crate) use cookies::*;
#[cfg(test)]
pub(crate) use errors::*;
#[cfg(test)]
pub(crate) use models::*;
#[cfg(test)]
pub(crate) use responses::*;
#[cfg(test)]
pub(crate) use server::*;
#[cfg(test)]
pub(crate) use service_tier::*;

#[cfg(test)]
mod tests;
