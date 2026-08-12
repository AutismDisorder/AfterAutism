// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 afterautism project contributors

//! Opaque identifier types: `NodeId` and `AdapterId`.

use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! opaque_id {
    ($name:ident, $prefix:literal) => {
        /// Opaque 64-bit identifier.
        /// The integer is meaningless outside this process; comparison and
        /// hashing define equality.
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[repr(transparent)]
        pub struct $name(u64);

        impl $name {
            /// Wrap a known identifier. Caller is responsible for
            /// uniqueness within the context they will use it in.
            #[must_use]
            #[inline]
            pub const fn from_raw(raw: u64) -> Self {
                Self(raw)
            }

            /// Recover the underlying integer. Public because some storage
            /// paths store identifiers as raw integers; do **not** use it
            /// when comparing identifiers across contexts where uniqueness
            /// rules differ.
            #[must_use]
            #[inline]
            pub const fn to_raw(self) -> u64 {
                self.0
            }

            /// Sentinel for an unknown identifier. Useful for "the adapter
            /// has not yet produced one" / "look this up and replace".
            #[must_use]
            #[inline]
            pub const fn unknown() -> Self {
                Self(u64::MAX)
            }

            /// True if this is the `unknown` sentinel.
            #[must_use]
            #[inline]
            pub const fn is_unknown(self) -> bool {
                self.0 == u64::MAX
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                // The prefix is what makes a log distinguish a `NodeId`
                // from an `AdapterId` without an extra `Debug` print.
                write!(f, concat!($prefix, "{:016x}"), self.0)
            }
        }
    };
}

opaque_id!(NodeId, "n");
opaque_id!(AdapterId, "a");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_id_round_trips() {
        let n = NodeId::from_raw(0xDEAD_BEEF);
        assert_eq!(n.to_raw(), 0xDEAD_BEEF);
        assert!(!n.is_unknown());
        assert_eq!(NodeId::unknown().to_raw(), u64::MAX);
        assert!(NodeId::unknown().is_unknown());
    }

    #[test]
    fn node_id_display_has_prefix() {
        let n = NodeId::from_raw(0xCAFE_F00D);
        let s = n.to_string();
        // `Debug` would be `NodeId(0x...)`; `Display` is our prefixed hex.
        assert!(
            s.starts_with('n'),
            "Node id display should start with 'n' prefix, got {s}"
        );
        assert!(
            s.contains("cafe"),
            "display should be lowercase hex, got {s}"
        );
    }

    #[test]
    fn adapter_id_distinguished_from_node_id() {
        let n = NodeId::from_raw(0x1234);
        let a = AdapterId::from_raw(0x1234);
        // The types are incompatible — the next line wouldn't compile if
        // the wrappers were missing:
        assert_ne!(n.to_string(), a.to_string());
        assert_eq!(n.to_string(), "n0000000000001234");
        assert_eq!(a.to_string(), "a0000000000001234");
    }

    #[test]
    fn ids_are_hashable_in_sets() {
        use std::collections::HashSet;
        let set: HashSet<NodeId> = [
            NodeId::from_raw(1),
            NodeId::from_raw(2),
            NodeId::from_raw(3),
            NodeId::from_raw(1), // duplicate; should be coalesced
        ]
        .into_iter()
        .collect();
        assert_eq!(set.len(), 3);
    }

    #[test]
    fn ids_serialize_as_inner_u64() {
        // Sent over serde for storage; we want exactly the inner integer
        // to cross the wire so the on-disk format stays stable across
        // potential identifier-type refactors.
        let n = NodeId::from_raw(0xABC);
        let serialized = serde_json::to_string(&n).expect("NodeId serializes");
        assert_eq!(serialized, "2748");
        let back: NodeId = serde_json::from_str(&serialized).expect("round-trips");
        assert_eq!(back, n);
    }

    #[test]
    fn unknown_sentinel_is_display_consistent() {
        let u = NodeId::unknown();
        assert!(u.to_string().starts_with('n'));
        assert!(u.is_unknown());
    }
}
