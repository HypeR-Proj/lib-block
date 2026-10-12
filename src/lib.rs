// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bounded userspace partition discovery, independent of transport and filesystem.
#![no_std]

extern crate alloc;

pub mod gpt;
mod identity;

pub use identity::{Guid, Selector};
