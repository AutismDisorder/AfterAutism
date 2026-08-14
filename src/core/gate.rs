// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 afterautism project contributors

//! `NetworkGate` — the single chokepoint for outbound network I/O.
//! Offline by default: egress requires an explicit `Online` policy.

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};

/// The runtime policy the gate enforces. Configured by the scale
/// controller (`aa-scale`) and read once at startup; the policy may
/// flip from `Online` to `Offline` during the run (e.g., the user
/// toggles a privacy mode UI switch), and the gate must then refuse
/// subsequent egress.
/// The reverse — flipping from `Offline` to `Online` mid-run — is also
/// allowed, since privacy mode is intended to be reversible. The gate
/// updates atomically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkPolicy {
    /// Outbound network egress is permitted for ops that opt in.
    Online,
    /// Outbound network egress is structurally refused.
    Offline,
}

impl Default for NetworkPolicy {
    /// Defaults to `Offline`: a missing config never results in
    /// unintended egress.
    fn default() -> Self {
        Self::Offline
    }
}

impl NetworkPolicy {
    /// True if the policy permits any network egress.
    #[must_use]
    pub const fn permits_egress(self) -> bool {
        matches!(self, Self::Online)
    }
}

/// The gate itself. Cheap to construct; one per application process is
/// expected, but the type is small enough to thread into helper contexts
/// that need to assert egress permission without juggling global state.
/// Interior mutability via an `AtomicBool` so policy updates are visible
/// everywhere the gate is read; cloning a `NetworkGate` clones the view
/// of the same atomic.
/// Important: an open gate is **necessary**, not **sufficient**. A path
/// that holds a gate permit may still need to respect other policies
/// (rate-limit, tier caps). The gate is the "is offline mode on?" knob
/// alone.
/// Per-domain egress policy.
/// Lets callers allow specific hosts through while the global policy is
/// `Offline` (e.g. "offline by default, but this corpora mirror host is
/// trusted"). Domain matches are suffix-based: `"crates.io"` also allows
/// `"static.crates.io"`. This is an *additional* allowance, never a
/// backdoor: when the global policy is `Online`, the allowlist is
/// irrelevant.
/// Each entry stores its two comparison forms (exact and dotted suffix)
/// at `allow` time, so per-check matching is allocation-free.
#[derive(Debug, Clone, Default)]
pub struct DomainPolicy {
    allowed: ArcDomainEntries,
}

impl DomainPolicy {
    /// Allow egress to `host` (suffix match) even while globally offline.
    pub fn allow(&self, host: impl Into<String>) {
        let host = host.into();
        let suffix = format!(".{host}");
        self.allowed.push((host, suffix));
    }

    /// True when `host` matches an allowed suffix.
    #[must_use]
    pub fn permits(&self, host: &str) -> bool {
        self.allowed
            .iter()
            .any(|(exact, suffix)| host == exact || host.ends_with(suffix.as_str()))
    }
}

/// A shared, append-only list of `(exact, dotted-suffix)` domain pairs.
#[derive(Debug, Clone, Default)]
struct ArcDomainEntries {
    inner: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
}

impl ArcDomainEntries {
    fn push(&self, value: (String, String)) {
        self.inner.lock().unwrap().push(value);
    }
    fn iter(&self) -> impl Iterator<Item = (String, String)> + '_ {
        let guard = self.inner.lock().unwrap();
        guard.clone().into_iter()
    }
}

/// The gate itself. Cheap to construct; one per application process is
/// expected, but the type is small enough to thread into helper contexts
/// that need to assert egress permission without juggling global state.
/// Interior mutability via an `AtomicBool` so policy updates are visible
/// everywhere the gate is read; cloning a `NetworkGate` clones the view
/// of the same atomic.
/// Important: an open gate is **necessary**, not **sufficient**. A path
/// that holds a gate permit may still need to respect other policies
/// (rate-limit, tier caps). The gate is the "is offline mode on?" knob
/// alone.
#[derive(Debug, Clone, Default)]
pub struct NetworkGate {
    /// Stored inverted so `false` (the zeros int) corresponds to the
    /// vanilla default policy (Offline) — a never-configured process
    /// lands on the safe side without writing.
    permits_egress: ArcFlag,
    /// Per-domain allowances that apply while the gate is offline.
    domain_policy: DomainPolicy,
}

impl NetworkGate {
    /// Build a gate around a starting policy. Test code can use
    /// `from_policy(NetworkPolicy::Online)` to allow egress.
    #[must_use]
    pub fn from_policy(policy: NetworkPolicy) -> Self {
        let gate = Self::default();
        gate.set_policy(policy);
        gate
    }

    /// Replace the runtime policy. Visible to all clones immediately.
    /// Reversible.
    pub fn set_policy(&self, policy: NetworkPolicy) {
        self.permits_egress.store(policy.permits_egress());
    }

    /// Inspect the current policy snapshot. Atomic with respect to clone
    /// reads via `swap()` semantics, but exposes the latest committed
    /// state.
    #[must_use]
    pub fn policy(&self) -> NetworkPolicy {
        if self.permits_egress.load() {
            NetworkPolicy::Online
        } else {
            NetworkPolicy::Offline
        }
    }

    /// Returns `Ok(())` if egress is permitted, else `Err`. The standard
    /// pre-egress check that downstream call sites call before performing
    /// any outbound I/O.
    pub fn try_open(&self) -> Result<(), NetworkGateError> {
        if self.permits_egress.load() {
            Ok(())
        } else {
            Err(NetworkGateError::OfflineEnforced)
        }
    }

    /// Convenience: predicate form. Useful in lazy iterator chains where
    /// we want to filter without allocating an error.
    #[must_use]
    pub fn allows_egress(&self) -> bool {
        self.permits_egress.load()
    }

    /// Per-domain egress check: passes when globally online, or when the
    /// domain is on the allowlist (offline-but-trusted-host mode).
    pub fn try_open_domain(&self, host: &str) -> Result<(), NetworkGateError> {
        if self.allows_egress() || self.domain_policy.permits(host) {
            Ok(())
        } else {
            Err(NetworkGateError::OfflineEnforced)
        }
    }

    /// Access the domain allowlist (e.g. to add trusted hosts).
    pub fn domain_policy(&self) -> &DomainPolicy {
        &self.domain_policy
    }
}

/// A simple shared flag backing the gate. `Arc` so clones observe the
/// same policy; `AtomicBool` so writes are visible without locking.
#[derive(Debug, Clone, Default)]
struct ArcFlag {
    flag: std::sync::Arc<AtomicBool>,
}

impl ArcFlag {
    fn store(&self, value: bool) {
        self.flag.store(value, Ordering::Release);
    }
    fn load(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }
}

/// Why egress was refused. Currently the only reason is "the gate is
/// closed because policy is Offline"; we keep it as an enum so adding
/// future refusals (rate-limit hit, tier-cap hit) doesn't break call
/// sites that match the variant.
#[derive(Debug, thiserror::Error)]
pub enum NetworkGateError {
    /// The runtime policy is `Offline` (the privacy / default mode).
    #[error("network egress refused: offline mode enforced")]
    OfflineEnforced,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_gate_refuses_egress() {
        // : privacy-by-default. A never-configured gate must
        // refuse, not permit, any egress.
        let gate = NetworkGate::default();
        assert!(!gate.allows_egress());
        assert!(matches!(
            gate.try_open(),
            Err(NetworkGateError::OfflineEnforced)
        ));
        assert_eq!(gate.policy(), NetworkPolicy::Offline);
    }

    #[test]
    fn from_policy_online_allows_egress() {
        let gate = NetworkGate::from_policy(NetworkPolicy::Online);
        assert!(gate.allows_egress());
        assert!(gate.try_open().is_ok());
    }

    #[test]
    fn flipping_policy_offline_takes_effect_immediately_and_in_clones() {
        // The switch must be reversible and observable to existing clones
        // — the privacy-mode UI toggle is reversible.
        let gate = NetworkGate::from_policy(NetworkPolicy::Online);
        let observer = gate.clone();
        assert!(observer.allows_egress());

        gate.set_policy(NetworkPolicy::Offline);
        assert!(!observer.allows_egress());
        assert!(matches!(
            observer.try_open(),
            Err(NetworkGateError::OfflineEnforced)
        ));

        // Flipping back online works too.
        gate.set_policy(NetworkPolicy::Online);
        assert!(observer.allows_egress());
    }

    #[test]
    fn policy_default_is_offline() {
        // Belt and braces for 's "default" guarantee: even if some
        // caller does `NetworkPolicy::default()` instead of constructing
        // from config, they should land on the safe side.
        assert_eq!(NetworkPolicy::default(), NetworkPolicy::Offline);
    }

    #[test]
    fn permits_egress_predicate_matches_enum() {
        assert!(NetworkPolicy::Online.permits_egress());
        assert!(!NetworkPolicy::Offline.permits_egress());
    }

    #[test]
    fn domain_allowlist_passes_while_offline() {
        let gate = NetworkGate::default(); // offline
        assert!(gate.try_open_domain("crates.io").is_err());
        gate.domain_policy().allow("crates.io");
        assert!(gate.try_open_domain("crates.io").is_ok());
        // Suffix match: subdomains allowed too.
        assert!(gate.try_open_domain("static.crates.io").is_ok());
        // Unrelated hosts still refused.
        assert!(gate.try_open_domain("evil.example.com").is_err());
    }

    #[test]
    fn allowlist_never_backdoors_global_offline_for_other_hosts() {
        let gate = NetworkGate::default();
        gate.domain_policy().allow("trusted.example.com");
        assert!(gate.try_open_domain("untrusted.example.com").is_err());
    }
}
