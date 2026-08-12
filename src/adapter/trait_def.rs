// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 afterautism project contributors

//! The [`Adapter`] trait — the extension contract of the engine.

use crate::adapter::error::AdapterError;
use crate::adapter::fields::NodeField;
use crate::adapter::record::{Edge, EdgeType, Node};
use crate::adapter::source::Source;
use crate::core::AdapterId;

/// What an adapter is. Trait object friendly: the runtime holds a
/// `Box<dyn Adapter>` and dispatches per-adapter ingest.
/// The trait is intentionally *minimal*. The ingest pipeline handler
/// invokes `ingest()`; the topology engine reads `edge_types()` to
/// interpret adapter-supplied typed edges generically. The adapter does
/// not own rendering, filtering, or storage; it only knows about its
/// own data type.
/// The current adapter ABI version.
/// Plugin hosts check the `afterautism_adapter_abi_version` symbol
/// (exported by `afterautism-adapter`) against this constant at bind time
/// and refuse to load a mismatched plugin instead of crashing on a vtable
/// mismatch.
pub const ADAPTER_ABI_VERSION: u32 = 1;

/// Typed capability descriptor for an adapter.
/// replaces the boolean `supports_full_page()` + implicit feature
/// assumptions with a structured descriptor the host can query before
/// driving an adapter. All fields default to "not supported"; adapters
/// override [`Adapter::capabilities`] to declare what they provide.
// A capability descriptor is a flag set; independent booleans are the
// correct representation (each flag gates a distinct behavior).
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AdapterCapabilities {
    /// Nodes can carry a full-page (heavy) payload marker.
    pub full_page: bool,
    /// The adapter can stream very large sources without loading them
    /// into memory (see `ingest_stream` — not yet in the trait).
    pub streaming: bool,
    /// The adapter emits stable, content-derived node ids (idempotent
    /// re-ingest produces identical ids).
    pub stable_ids: bool,
    /// The adapter can ingest incrementally (delta since a previous
    /// `SourceMeta`).
    pub incremental: bool,
    /// The adapter produces binary-safe batches (payloads that are not
    /// valid UTF-8).
    pub binary_safe: bool,
    /// The adapter declares expected resource cost (nodes/byte) for
    /// budgeting before ingest.
    pub resource_hints: bool,
}

#[allow(clippy::derivable_impls)]
impl Default for AdapterCapabilities {
    fn default() -> Self {
        Self {
            full_page: false,
            streaming: false,
            stable_ids: false,
            incremental: false,
            binary_safe: false,
            resource_hints: false,
        }
    }
}

impl AdapterCapabilities {
    /// A capability set with every flag set (for tests and maximal
    /// adapters).
    #[must_use]
    pub const fn all() -> Self {
        Self {
            full_page: true,
            streaming: true,
            stable_ids: true,
            incremental: true,
            binary_safe: true,
            resource_hints: true,
        }
    }
}

pub trait Adapter: Send + Sync {
    /// Adapter's stable identifier. Two adapters in the same runtime
    /// must not collide.
    fn id(&self) -> AdapterId;

    /// Human-readable name for this adapter. Free-form; surfaced in
    /// logs and UI.
    fn name(&self) -> &str;

    /// The catalogue of edge types this adapter emits, with one-line
    /// human descriptions of each (used in the filter UI for
    /// discoverability).
    fn edge_types(&self) -> &[(EdgeType, &str)];

    /// Convenience: predicate "does this adapter emit edges of this
    /// type?" Adapters that don't override get the default-scoped
    /// implementation.
    fn emits_edge_type(&self, edge_type: &EdgeType) -> bool {
        self.edge_types().iter().any(|(t, _)| t == edge_type)
    }

    /// Whether the produced nodes can render as full pages or are
    /// text-only. Web pages: yes; structured CSV records: no.
    fn supports_full_page(&self) -> bool {
        false
    }

    /// The typed capability descriptor for this adapter.
    /// Defaults derive from the legacy booleans so existing adapters keep
    /// working: `full_page` mirrors [`Self::supports_full_page`].
    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities {
            full_page: self.supports_full_page(),
            ..Default::default()
        }
    }

    /// Ingest one [`Source`] into an [`IngestBatch`].
    /// The default implementation returns
    /// [`AdapterError::Unsupported`] so minimal adapters keep
    /// compiling; new adapters override this method.
    /// The contract is **synchronous** and **source-carrying** rather
    /// than `async fn ingest(&str)`:
    /// - Sync: adapter plugins are `cdylib`s loaded with `libloading`;
    ///   an async method would require a runtime inside every plugin.
    /// - `&Source` not `&str`: binary formats (PDF, DOCX, XLSX) cannot
    ///   travel through a UTF-8 string.
    ///
    /// Adapters that read from a local path may use `source.key`;
    /// adapters fed by the refresh pipeline receive content through
    /// their own fetch path. Either way, identity + metadata travel in
    /// the `Source`.
    fn ingest(&self, source: &Source) -> Result<IngestBatch, AdapterError> {
        let _ = source;
        Err(AdapterError::Unsupported)
    }
}

/// Symbiotic type — the data shape an adapter emits when triggered.
/// Stored as a small batch rather than a stream so the storage layer can
/// apply atomic swaps per / .
#[derive(Debug, Clone, Default)]
pub struct IngestBatch {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    /// Typed fields attached to nodes (form-declared semantics).
    pub fields: Vec<NodeField>,
}

impl IngestBatch {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Convenience constructor used by tests and small adapters.
    #[must_use]
    pub fn from(nodes: Vec<Node>, edges: Vec<Edge>) -> Self {
        Self {
            nodes,
            edges,
            fields: Vec::new(),
        }
    }

    /// Predicate. An empty batch is valid (e.g., for an incremental
    /// refresh that finds no updates), callers shouldn't assume the
    /// collection is non-empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty() && self.edges.is_empty()
    }

    /// Push a node and chainable return.
    pub fn push_node(&mut self, node: Node) -> &mut Self {
        self.nodes.push(node);
        self
    }

    /// Push an edge and chainable return.
    pub fn push_edge(&mut self, edge: Edge) -> &mut Self {
        self.edges.push(edge);
        self
    }

    /// Attach a typed field to a node and chainable return.
    pub fn push_field(&mut self, field: NodeField) -> &mut Self {
        self.fields.push(field);
        self
    }
}

// The public surface re-export is handled by `lib.rs` (`pub use record::...`).
// Adding per-symbol convenience re-exports here would clutter the trait
// module's namespace and force adapter authors to navigate two paths
// for the same type. Keep this module strictly about the `Adapter` trait
// and its symbiotic `IngestBatch`.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_capabilities_are_all_false() {
        let caps = AdapterCapabilities::default();
        assert!(!caps.full_page && !caps.streaming && !caps.stable_ids);
        assert!(!caps.incremental && !caps.binary_safe && !caps.resource_hints);
    }

    #[test]
    fn all_capabilities_everything_on() {
        let caps = AdapterCapabilities::all();
        assert!(caps.full_page && caps.streaming && caps.stable_ids);
        assert!(caps.incremental && caps.binary_safe && caps.resource_hints);
    }

    #[test]
    fn abi_version_is_stable_at_one() {
        assert_eq!(ADAPTER_ABI_VERSION, 1);
    }

    #[test]
    fn capabilities_serialize_snake_case() {
        let caps = AdapterCapabilities::all();
        let json = serde_json::to_string(&caps).expect("serialize");
        assert!(json.contains("full_page"));
        assert!(json.contains("stable_ids"));
    }

    #[test]
    fn default_capabilities_mirror_supports_full_page() {
        // A struct whose Adapter impl overrides supports_full_page must
        // surface it through capabilities().
        #[derive(Debug)]
        struct PageAdapter;
        impl Adapter for PageAdapter {
            fn id(&self) -> AdapterId {
                AdapterId::from_raw(0x0bad)
            }
            fn name(&self) -> &'static str {
                "pages"
            }
            fn edge_types(&self) -> &[(EdgeType, &str)] {
                &[]
            }
            fn supports_full_page(&self) -> bool {
                true
            }
        }
        let caps = PageAdapter.capabilities();
        assert!(caps.full_page);
        assert!(!caps.streaming);
    }
}
