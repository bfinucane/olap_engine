//! The security file: users, roles and password verifiers.
//!
//! WHY A SEPARATE FILE, NOT THE CATALOGUE
//!   The catalogue is bincode — a compact format with no field names or versioning per
//!   record. Adding `users` to `Catalog` would make every existing `database.bin` fail to
//!   load (the loader REFUSES a newer format rather than misreading it, which is right,
//!   but it means a breaking change). Credentials also do not belong in the data file.
//!
//!   So identity lives beside the database, in `security.json`:
//!     * readable and fixable by hand (a locked-out admin is a text edit, not a tool),
//!     * never in a cube, because cubes are queryable and these records are secrets,
//!     * absent file = no users = no access (when auth is required), which fails closed.
//!
//! USERS -> ROLES. A user holds role NAMES; the file also keeps the set of roles that exist
//! so a typo is visible. Permission data itself (what a role may read) lands in the S2
//! control cubes; this file is identity only.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::hash::PasswordRecord;

/// One user. The password is a `pbkdf2-sha256$iterations$salt$hash` textual record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UserRecord {
    pub name: String,
    #[serde(default)]
    pub roles: Vec<String>,
    /// Empty means "no password can match" — a user with no password cannot log in.
    #[serde(default)]
    pub password: String,
}

/// The on-disk file.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecurityFile {
    #[serde(default)]
    pub users: Vec<UserRecord>,
    /// The roles that exist. Kept sorted so the file diffs cleanly.
    #[serde(default)]
    pub roles: Vec<String>,
}

impl SecurityFile {
    // ---- loading and saving -------------------------------------------------

    /// Loads the file. A missing file is an EMPTY security file (no users), not an error:
    /// that is the state before the first `--add-user`, and callers decide what it means.
    pub fn load(path: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|e| format!("security file '{}' is not valid JSON: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("could not read security file '{}': {e}", path.display())),
        }
    }

    /// Saves atomically: write a sibling temp file, then rename over the target.
    ///
    /// A half-written security file is a locked-out admin, so this is not a place to be
    /// casual about crashes.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| format!("could not serialise the security file: {e}"))?;

        let temp = temp_path(path);
        std::fs::write(&temp, text.as_bytes())
            .map_err(|e| format!("could not write '{}': {e}", temp.display()))?;
        std::fs::rename(&temp, path).map_err(|e| {
            let _ = std::fs::remove_file(&temp);
            format!("could not replace '{}': {e}", path.display())
        })
    }

    // ---- queries ------------------------------------------------------------

    /// Finds a user by name, case-insensitively (logins should not be case games).
    pub fn user(&self, name: &str) -> Option<&UserRecord> {
        self.users
            .iter()
            .find(|user| user.name.eq_ignore_ascii_case(name))
    }

    pub fn is_empty(&self) -> bool {
        self.users.is_empty()
    }

    /// Checks a login. Returns the roles on success.
    ///
    /// An unknown user still performs one password verification against a dummy record, so
    /// login latency does not reveal which half of the credentials was wrong.
    pub fn authenticate(&self, name: &str, password: &str) -> Option<Vec<String>> {
        match self.user(name) {
            Some(user) => {
                let record = PasswordRecord::parse(&user.password)?;
                if record.verify(password) {
                    Some(user.roles.clone())
                } else {
                    None
                }
            }
            None => {
                // Burn comparable time, then fail. The record is fixed so the cost is
                // constant rather than dependent on the (secret) real records.
                let decoy = PasswordRecord {
                    iterations: super::hash::DEFAULT_ITERATIONS,
                    salt_hex: "00112233445566778899aabbccddeeff".to_string(),
                    hash_hex: "00".repeat(32),
                };
                let _ = decoy.verify(password);
                None
            }
        }
    }

    // ---- mutation -----------------------------------------------------------

    /// Creates or updates a user. Roles are created on demand, so `--add-user alice
    /// --role reader` needs no separate "make role" step.
    pub fn upsert_user(&mut self, name: &str, password: &str, roles: &[String]) -> Result<(), String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("a user needs a name".to_string());
        }
        if password.is_empty() {
            return Err("a user needs a non-empty password".to_string());
        }

        let record = PasswordRecord::create(password).to_string_form();
        let roles: Vec<String> = roles
            .iter()
            .map(|role| role.trim().to_string())
            .filter(|role| !role.is_empty())
            .collect();

        let mut known: BTreeSet<String> = self.roles.iter().cloned().collect();
        for role in &roles {
            if known.insert(role.clone()) {
                self.roles.push(role.clone());
            }
        }

        match self.users.iter_mut().find(|u| u.name.eq_ignore_ascii_case(name)) {
            Some(existing) => {
                // Update in place, keeping the stored casing of the name.
                existing.password = record;
                existing.roles = roles;
            }
            None => self.users.push(UserRecord {
                name: name.to_string(),
                roles,
                password: record,
            }),
        }
        self.roles.sort();
        self.roles.dedup();
        Ok(())
    }

    /// Removes a user. Returns whether it existed.
    pub fn remove_user(&mut self, name: &str) -> bool {
        let before = self.users.len();
        self.users.retain(|user| !user.name.eq_ignore_ascii_case(name));
        before != self.users.len()
    }
}

/// Sibling temp path for a security file (same directory, so the rename is atomic).
fn temp_path(path: &Path) -> PathBuf {
    let mut temp = path.to_path_buf();
    let name = temp
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "security.json".to_string());
    temp.set_file_name(format!(".{name}.tmp"));
    temp
}

/// The default security file: beside the database, named `security.json`.
pub fn default_path(database_path: &Path) -> PathBuf {
    let mut path = database_path.to_path_buf();
    path.set_file_name("security.json");
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_with_alice() -> SecurityFile {
        let mut file = SecurityFile::default();
        file.upsert_user("alice", "wonderland", &["reader".to_string()])
            .expect("create alice");
        file
    }

    #[test]
    fn a_user_can_log_in_with_the_right_password_only() {
        let file = file_with_alice();
        assert_eq!(
            file.authenticate("alice", "wonderland"),
            Some(vec!["reader".to_string()])
        );
        assert_eq!(file.authenticate("alice", "Wonderland"), None);
        assert_eq!(file.authenticate("alice", ""), None);
        assert_eq!(file.authenticate("nobody", "wonderland"), None);
    }

    #[test]
    fn user_names_are_matched_case_insensitively() {
        let file = file_with_alice();
        assert!(file.authenticate("ALICE", "wonderland").is_some());
    }

    #[test]
    fn roles_are_created_on_demand_and_kept_sorted() {
        let mut file = SecurityFile::default();
        file.upsert_user("bob", "pw-one", &["zeta".to_string(), "alpha".to_string()])
            .expect("bob");
        file.upsert_user("carol", "pw-two", &["alpha".to_string(), "beta".to_string()])
            .expect("carol");
        assert_eq!(file.roles, vec!["alpha", "beta", "zeta"]);
    }

    #[test]
    fn upsert_updates_an_existing_user_without_duplicating() {
        let mut file = file_with_alice();
        file.upsert_user("alice", "new-password", &["admin".to_string()])
            .expect("update");
        assert_eq!(file.users.len(), 1);
        assert_eq!(file.users[0].name, "alice", "stored casing is kept");
        assert!(file.authenticate("alice", "new-password").is_some());
        assert!(file.authenticate("alice", "wonderland").is_none());
        assert!(file.roles.contains(&"admin".to_string()));
    }

    #[test]
    fn a_user_with_no_password_record_cannot_log_in() {
        let mut file = SecurityFile::default();
        file.users.push(UserRecord {
            name: "ghost".to_string(),
            roles: vec![],
            password: String::new(),
        });
        assert_eq!(file.authenticate("ghost", ""), None, "empty record must not match");
        assert_eq!(file.authenticate("ghost", "anything"), None);
    }

    #[test]
    fn an_unparseable_password_record_fails_closed() {
        let mut file = SecurityFile::default();
        file.users.push(UserRecord {
            name: "broken".to_string(),
            roles: vec!["admin".to_string()],
            password: "argon2id$not$supported".to_string(),
        });
        assert_eq!(file.authenticate("broken", ""), None);
        assert_eq!(file.authenticate("broken", "argon2id$not$supported"), None);
    }

    #[test]
    fn remove_user_reports_whether_it_existed() {
        let mut file = file_with_alice();
        assert!(file.remove_user("alice"));
        assert!(!file.remove_user("alice"));
        assert!(file.is_empty());
    }

    #[test]
    fn empty_names_and_passwords_are_refused() {
        let mut file = SecurityFile::default();
        assert!(file.upsert_user("  ", "pw", &[]).is_err());
        assert!(file.upsert_user("dave", "", &[]).is_err());
    }

    #[test]
    fn the_file_round_trips_through_json() {
        let file = file_with_alice();
        let text = serde_json::to_string_pretty(&file).expect("serialise");
        let back: SecurityFile = serde_json::from_str(&text).expect("deserialise");
        assert_eq!(back, file);
        // The password field must be a verifier, never the password.
        assert!(!text.contains("wonderland"), "plaintext password in the file: {text}");
        assert!(text.contains("pbkdf2-sha256$"), "{text}");
    }

    #[test]
    fn a_missing_file_loads_as_empty_and_a_broken_one_errors() {
        let dir = std::env::temp_dir().join(format!("olap-sec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("missing.json");

        let loaded = SecurityFile::load(&path).expect("missing file is an empty security file");
        assert!(loaded.is_empty());

        let broken = dir.join("broken.json");
        std::fs::write(&broken, b"{ not json").expect("write");
        assert!(SecurityFile::load(&broken).is_err(), "a broken file must not be read as empty");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_then_load_preserves_the_users() {
        let dir = std::env::temp_dir().join(format!("olap-sec-save-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("security.json");

        let file = file_with_alice();
        file.save(&path).expect("save");
        let back = SecurityFile::load(&path).expect("load");
        assert_eq!(back, file);
        assert!(back.authenticate("alice", "wonderland").is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_default_security_path_sits_beside_the_database() {
        let path = default_path(Path::new("/data/olap/database.bin"));
        assert_eq!(path, PathBuf::from("/data/olap/security.json"));
    }
}
