//! ctx's projects and the work in them: the store, item refresh, wait conditions, search,
//! Claude summaries and review responses. Front ends such as the ctx TUI build on this crate and
//! give it their terminals through [`terminals::Terminals`].

pub mod brief;
pub mod cache;
pub mod changes;
pub mod config;
pub mod inbox;
pub mod item;
pub mod live;
pub mod ops;
pub mod paths;
pub mod refresh;
pub mod respond;
pub mod search;
pub mod store;
pub mod terminals;
pub mod wait;
pub mod watch;
