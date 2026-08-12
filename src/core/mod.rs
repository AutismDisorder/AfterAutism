// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 afterautism project contributors

//! Core types: opaque ids (`NodeId`, `AdapterId`), the shared error
//! model, and the `NetworkGate` — the single chokepoint for outbound
//! network I/O, offline by default.

pub mod error;
pub mod gate;
pub mod ids;

pub use error::{Error, Result};
pub use gate::{NetworkGate, NetworkPolicy};
pub use ids::{AdapterId, NodeId};
