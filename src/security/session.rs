//! Session tokens: the thing a client carries after logging in.
//!
//! Sessions live in MEMORY only. A restart logs everybody out, which is the honest
//! behaviour for a tool with no persistent token store: a stolen token dies with the
//! process, and there is no way for a token to outlive the session that issued it.
//!
//! A token is 32 random bytes in hex. It is never written to disk and never logged; the
//! only thing the server stores is the token itself, keyed in a map with its session.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use super::hash::random_bytes;

/// A logged-in identity: who, what they may do (roles), and when they must log in again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub user: String,
    pub roles: Vec<String>,
    /// Unix seconds at which the session stops being valid.
    pub expires_at: u64,
}

impl Session {
    /// Convenience for display: the role list as one string.
    pub fn roles_text(&self) -> String {
        if self.roles.is_empty() {
            "(no roles)".to_string()
        } else {
            self.roles.join(", ")
        }
    }
}

/// In-memory session table, safe to share across connection threads.
pub struct SessionStore {
    sessions: Mutex<HashMap<String, Session>>,
    ttl_secs: u64,
}

impl SessionStore {
    pub fn new(ttl_secs: u64) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            ttl_secs,
        }
    }

    /// Issues a fresh token for an authenticated identity.
    pub fn issue(&self, user: &str, roles: Vec<String>) -> (String, Session) {
        let token = super::hash::to_hex(&random_bytes(32));
        let session = Session {
            user: user.to_string(),
            roles,
            expires_at: now_unix().saturating_add(self.ttl_secs),
        };
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        sessions.insert(token.clone(), session.clone());
        (token, session)
    }

    /// Validates a token. Expired tokens are dropped on the way out, so the table does not
    /// grow forever in a long-running server.
    pub fn validate(&self, token: &str) -> Option<Session> {
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        match sessions.get(token) {
            Some(session) if now_unix() < session.expires_at => Some(session.clone()),
            Some(_) => {
                sessions.remove(token);
                None
            }
            None => None,
        }
    }

    /// Drops a token (logout). Returns whether it was there.
    pub fn revoke(&self, token: &str) -> bool {
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        sessions.remove(token).is_some()
    }

    pub fn len(&self) -> usize {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn ttl_secs(&self) -> u64 {
        self.ttl_secs
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_issued_token_validates_to_its_session() {
        let store = SessionStore::new(3600);
        let (token, issued) = store.issue("alice", vec!["reader".to_string()]);
        let found = store.validate(&token).expect("valid token");
        assert_eq!(found, issued);
        assert_eq!(found.user, "alice");
        assert_eq!(found.roles, vec!["reader".to_string()]);
        assert!(found.expires_at > now_unix());
    }

    #[test]
    fn an_unknown_token_is_rejected() {
        let store = SessionStore::new(3600);
        store.issue("alice", vec![]);
        assert!(store.validate("deadbeef").is_none());
        assert!(store.validate("").is_none());
    }

    #[test]
    fn an_expired_token_is_rejected_and_removed() {
        // A zero TTL means the session is already expired at issue time.
        let store = SessionStore::new(0);
        let (token, _) = store.issue("alice", vec![]);
        assert!(store.validate(&token).is_none());
        assert_eq!(store.len(), 0, "expiry must drop the entry, not just reject it");
    }

    #[test]
    fn tokens_are_long_random_and_distinct() {
        let store = SessionStore::new(3600);
        let (a, _) = store.issue("alice", vec![]);
        let (b, _) = store.issue("alice", vec![]);
        assert_eq!(a.len(), 64, "32 bytes as hex");
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn revoking_a_token_logs_it_out() {
        let store = SessionStore::new(3600);
        let (token, _) = store.issue("alice", vec![]);
        assert!(store.revoke(&token));
        assert!(store.validate(&token).is_none());
        assert!(!store.revoke(&token), "second revoke reports it was gone");
    }

    #[test]
    fn roles_text_is_readable_when_empty() {
        let session = Session {
            user: "bob".to_string(),
            roles: vec![],
            expires_at: 0,
        };
        assert_eq!(session.roles_text(), "(no roles)");
    }
}
