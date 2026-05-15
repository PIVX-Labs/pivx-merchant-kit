//! pivx-merchant-kit — self-hosted PIVX payment processor.
//!
//! This crate is structured as both a library and a binary. The binary is the
//! daemon; the library exposes its modules for unit + integration testing and
//! for embedders who want to drive the lifecycle programmatically.

pub mod api;
pub mod cli;
pub mod config;
pub mod error;
pub mod invoice;
pub mod matcher;
pub mod payment;
pub mod storage;
pub mod sync;
pub mod wallet;
pub mod webhooks;
