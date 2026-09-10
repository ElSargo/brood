//! Linux session-capsule protocol, planning, and supervisor foundations.

#![cfg(target_os = "linux")]

pub mod broker;
pub mod host;
pub mod init;
pub mod plan;
pub mod protocol;
pub mod transport;

pub mod cli;
