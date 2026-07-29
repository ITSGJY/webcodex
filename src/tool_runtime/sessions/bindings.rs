//! Current-session bindings: bind / unbind / lookup.
//!
//! Raw exact keys stay in a process-local cache. Only their domain-separated
//! composite hashes enter the durable Workflow Session ledger.

use super::model::{CurrentSessionKey, SessionSummary};
use super::store::SessionStore;

impl SessionStore {
    pub(crate) fn bind_current_session(
        &self,
        key: CurrentSessionKey,
        session_id: &str,
    ) -> Option<SessionSummary> {
        let bound = {
            let mut inner = self.inner.lock().expect("session store mutex poisoned");
            inner.bind_current(key, session_id)
        };
        if bound.is_some() {
            self.persist_after_mutation();
        }
        bound
    }

    pub(crate) fn current_session(&self, key: &CurrentSessionKey) -> Option<SessionSummary> {
        let (summary, durable_changed) = {
            let mut inner = self.inner.lock().expect("session store mutex poisoned");
            inner.current_session(key)
        };
        if durable_changed {
            self.persist_after_mutation();
        }
        summary
    }

    pub(crate) fn current_session_id(&self, key: &CurrentSessionKey) -> Option<String> {
        self.current_session(key).map(|summary| summary.session_id)
    }

    pub(crate) fn unbind_current_session(&self, key: &CurrentSessionKey) -> bool {
        let removed = {
            let mut inner = self.inner.lock().expect("session store mutex poisoned");
            inner.unbind_current(key)
        };
        if removed {
            self.persist_after_mutation();
        }
        removed
    }
}
