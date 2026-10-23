//! Security: identity, credentials and sessions.
//!
//! Replaces the design's argon2 with PBKDF2-HMAC-SHA256, because this workspace cannot
//! reach crates.io (see `hash`). Password records carry an algorithm tag, so the choice
//! can be upgraded per user later without a file-format change.

pub mod access;
pub mod hash;
pub mod session;
pub mod store;