//! Data access shared by rvt and ctx: the process runner, the Azure DevOps and GitLab clients,
//! `claude -p`, and the item model.

pub mod ado;
pub mod claude;
pub mod config;
pub mod exec;
pub mod git;
pub mod gitlab;
pub mod model;
