//! # revm-statetest-types
//!
//! This crate provides type definitions and utilities for Ethereum state tests,
//! specifically tailored for use with REVM.
//!
//! It includes structures for representing account information, environment settings,
//! test cases, and transaction data used in Ethereum state tests.

#![no_std]

extern crate alloc;

// Re-export commonly used alloc types
pub use alloc::{
    collections::BTreeMap,
    string::String,
    vec::Vec,
};

mod account_info;
mod deserializer;
mod env;
mod spec;
mod test;
mod test_authorization;
mod test_suite;
mod test_unit;
mod transaction;

pub use account_info::*;
pub use deserializer::*;
pub use env::*;
pub use spec::*;
pub use test::*;
pub use test_authorization::*;
pub use test_suite::*;
pub use test_unit::*;
pub use transaction::*;
