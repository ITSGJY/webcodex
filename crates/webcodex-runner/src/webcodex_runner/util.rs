//! Small transport-agnostic helpers shared across the runner crate.

/// Return `true` if `haystack` contains any of `needles` as a substring.
///
/// Used by the error-classification helpers in [`crate::main`] (proxy/gateway
/// detection, connection-refused detection, TLS/auth failure detection) and
/// by the agent-transport error classifier in [`crate::webcodex_runner::transport`].
/// Both sites previously carried a byte-identical private copy of this one
/// liner; it has no behavioral coupling to either caller, so it lives here.
pub(crate) fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}
