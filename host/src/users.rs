//! Accounts in SQLite (`STATE_DIR/users.sqlite3`): user name, e-mail, Argon2id password hash,
//! role and an optional linked OpenID Connect identity (issuer + subject).
//!
//! One connection per process behind a mutex; WAL mode lets the CLI change accounts while the
//! server runs. Sessions record the account's `session_version`, which changes with its password
//! and when it is deleted, so those changes end existing sessions in every process.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::access::Role;
use crate::timeutil::{now_epoch, now_iso};

pub const DB_FILE: &str = "users.sqlite3";
pub const MIN_PASSWORD_LEN: usize = 8;
/// Successful Basic credentials are remembered briefly so each request does not pay for Argon2.
const VERIFIED_CACHE_SECS: i64 = 300;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS users (
    -- AUTOINCREMENT: ids of deleted accounts are never reused, so an old session cannot match
    -- a new account.
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    username TEXT NOT NULL UNIQUE COLLATE NOCASE,
    email TEXT UNIQUE COLLATE NOCASE,
    password_hash TEXT,
    role TEXT NOT NULL CHECK (role IN ('user', 'admin')),
    oidc_issuer TEXT,
    oidc_subject TEXT,
    session_version INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL,
    last_login_at TEXT,
    UNIQUE (oidc_issuer, oidc_subject),
    CHECK ((oidc_issuer IS NULL) = (oidc_subject IS NULL))
);
";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OidcLink {
    pub issuer: String,
    pub subject: String,
}

/// An account as shown to administrators; never carries the password hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub email: Option<String>,
    pub role: Role,
    pub has_password: bool,
    pub oidc: Option<OidcLink>,
    pub created_at: String,
    pub last_login_at: Option<String>,
    #[serde(skip)]
    pub session_version: i64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum UserError {
    Invalid(String),
    Conflict(String),
    NotFound,
    /// The change would leave no administrator.
    LastAdmin,
    Db(String),
}

impl std::fmt::Display for UserError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UserError::Invalid(m) | UserError::Conflict(m) | UserError::Db(m) => f.write_str(m),
            UserError::NotFound => f.write_str("no such user"),
            UserError::LastAdmin => f.write_str("at least one administrator must remain"),
        }
    }
}

impl std::error::Error for UserError {}

impl From<rusqlite::Error> for UserError {
    fn from(e: rusqlite::Error) -> Self {
        if let rusqlite::Error::SqliteFailure(f, Some(msg)) = &e
            && f.code == rusqlite::ErrorCode::ConstraintViolation
        {
            let what = if msg.contains("username") {
                "this user name is taken"
            } else if msg.contains("email") {
                "this e-mail address belongs to another user"
            } else if msg.contains("oidc") {
                "this sign-in identity is linked to another user"
            } else {
                "conflicting values"
            };
            return UserError::Conflict(what.into());
        }
        UserError::Db(e.to_string())
    }
}

pub struct NewUser<'a> {
    pub username: &'a str,
    pub email: Option<&'a str>,
    pub password: Option<&'a str>,
    pub role: Role,
}

/// Fields to change; `None` leaves a field as it is. `email: Some(None)` clears it.
#[derive(Default)]
pub struct UserUpdate<'a> {
    pub email: Option<Option<&'a str>>,
    pub role: Option<Role>,
    pub password: Option<&'a str>,
}

/// What the identity provider says about the person signing in.
pub struct OidcClaims<'a> {
    pub issuer: &'a str,
    pub subject: &'a str,
    pub username: &'a str,
    pub email: Option<&'a str>,
    pub email_verified: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum OidcOutcome {
    Existing(User),
    /// Linked to an account that had this verified e-mail address and no identity yet
    /// (`link_by_email`).
    Linked(User),
    Created(User),
    /// Unknown identity and `create_users = false`.
    NoAccount,
}

pub fn valid_username(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && name.len() <= 64
        && chars.all(|c| c.is_ascii_alphanumeric() || "._@-".contains(c))
}

pub fn valid_email(email: &str) -> bool {
    email.len() <= 254
        && !email.chars().any(|c| c.is_whitespace() || c.is_control())
        && email
            .split_once('@')
            .is_some_and(|(l, d)| !l.is_empty() && d.contains('.') && !d.contains('@'))
}

pub fn hash_password(password: &str) -> anyhow::Result<String> {
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|h| h.to_string())
        .map_err(|e| anyhow::anyhow!("cannot hash password: {e}"))
}

fn check_password(password: &str) -> Result<(), UserError> {
    if password.chars().count() < MIN_PASSWORD_LEN {
        return Err(UserError::Invalid(format!(
            "passwords need at least {MIN_PASSWORD_LEN} characters"
        )));
    }
    Ok(())
}

fn normalize_email(email: Option<&str>) -> Result<Option<String>, UserError> {
    match email.map(str::trim).filter(|e| !e.is_empty()) {
        None => Ok(None),
        Some(e) if valid_email(e) => Ok(Some(e.to_string())),
        Some(e) => Err(UserError::Invalid(format!(
            "{e:?} is not an e-mail address"
        ))),
    }
}

const COLUMNS: &str = "id, username, email, password_hash IS NOT NULL, role, oidc_issuer, \
                       oidc_subject, created_at, last_login_at, session_version";

fn user_from_row(r: &Row<'_>) -> rusqlite::Result<User> {
    let role: String = r.get(4)?;
    let issuer: Option<String> = r.get(5)?;
    let subject: Option<String> = r.get(6)?;
    Ok(User {
        id: r.get(0)?,
        username: r.get(1)?,
        email: r.get(2)?,
        has_password: r.get(3)?,
        role: Role::parse(&role).unwrap_or(Role::User),
        oidc: issuer
            .zip(subject)
            .map(|(issuer, subject)| OidcLink { issuer, subject }),
        created_at: r.get(7)?,
        last_login_at: r.get(8)?,
        session_version: r.get(9)?,
    })
}

pub struct UserDb {
    path: PathBuf,
    conn: Mutex<Connection>,
    /// Basic credentials verified recently: key = SHA-256 of user, password and hash.
    verified: Mutex<HashMap<[u8; 32], i64>>,
}

impl UserDb {
    /// Open (and create) the database in `state_dir`. A new file gets mode 0660: password hashes
    /// are readable by the server's user and group only.
    pub fn open(state_dir: &Path) -> Result<Self> {
        let path = state_dir.join(DB_FILE);
        let created = !path.exists();
        let conn = Connection::open(&path).with_context(|| format!("open {}", path.display()))?;
        if created {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o660))
                .with_context(|| format!("chmod {}", path.display()))?;
        }
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)
            .with_context(|| format!("create schema in {}", path.display()))?;
        conn.pragma_update(None, "user_version", 1)?;
        Ok(Self {
            path,
            conn: Mutex::new(conn),
            verified: Mutex::new(HashMap::new()),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn list(&self) -> Result<Vec<User>, UserError> {
        let conn = self.conn.lock();
        let mut st = conn.prepare_cached(&format!(
            "SELECT {COLUMNS} FROM users ORDER BY username COLLATE NOCASE"
        ))?;
        let users = st.query_map([], user_from_row)?.collect::<Result<_, _>>()?;
        Ok(users)
    }

    pub fn get(&self, id: i64) -> Result<Option<User>, UserError> {
        let conn = self.conn.lock();
        let mut st = conn.prepare_cached(&format!("SELECT {COLUMNS} FROM users WHERE id = ?1"))?;
        Ok(st.query_row([id], user_from_row).optional()?)
    }

    pub fn by_username(&self, username: &str) -> Result<Option<User>, UserError> {
        let conn = self.conn.lock();
        let mut st =
            conn.prepare_cached(&format!("SELECT {COLUMNS} FROM users WHERE username = ?1"))?;
        Ok(st.query_row([username], user_from_row).optional()?)
    }

    pub fn count_admins(&self) -> Result<i64, UserError> {
        let conn = self.conn.lock();
        Ok(
            conn.query_row("SELECT count(*) FROM users WHERE role = 'admin'", [], |r| {
                r.get(0)
            })?,
        )
    }

    /// The account if the password is right. Constant work for unknown users and accounts
    /// without a password: a dummy hash is verified so timing does not reveal names.
    pub fn verify_password(
        &self,
        username: &str,
        password: &str,
    ) -> Result<Option<User>, UserError> {
        let row = {
            let conn = self.conn.lock();
            let mut st = conn.prepare_cached(&format!(
                "SELECT {COLUMNS}, password_hash FROM users WHERE username = ?1"
            ))?;
            st.query_row([username], |r| {
                Ok((user_from_row(r)?, r.get::<_, Option<String>>(10)?))
            })
            .optional()?
        };
        let hash = row.as_ref().and_then(|(_, h)| h.clone());
        let key: [u8; 32] = Sha256::digest(
            format!("{username}\0{password}\0{}", hash.as_deref().unwrap_or("")).as_bytes(),
        )
        .into();
        let now = now_epoch();
        if hash.is_some()
            && self
                .verified
                .lock()
                .get(&key)
                .is_some_and(|t| now - t < VERIFIED_CACHE_SECS)
        {
            return Ok(row.map(|(u, _)| u));
        }
        static DUMMY: std::sync::LazyLock<String> =
            std::sync::LazyLock::new(|| hash_password("timing equaliser").unwrap_or_default());
        let checked = hash.as_deref().unwrap_or(DUMMY.as_str());
        let ok = PasswordHash::new(checked).is_ok_and(|h| {
            Argon2::default()
                .verify_password(password.as_bytes(), &h)
                .is_ok()
        }) && hash.is_some();
        if !ok {
            return Ok(None);
        }
        let mut verified = self.verified.lock();
        verified.retain(|_, t| now - *t < VERIFIED_CACHE_SECS);
        verified.insert(key, now);
        Ok(row.map(|(u, _)| u))
    }

    pub fn create(&self, new: NewUser<'_>) -> Result<User, UserError> {
        let username = new.username.trim();
        if !valid_username(username) {
            return Err(UserError::Invalid(
                "user names are 1-64 letters, digits, '.', '_', '@' or '-', starting with a letter or digit".into(),
            ));
        }
        let email = normalize_email(new.email)?;
        let hash = match new.password {
            Some(p) => {
                check_password(p)?;
                Some(hash_password(p).map_err(|e| UserError::Db(e.to_string()))?)
            }
            None => None,
        };
        self.insert(username, email.as_deref(), hash.as_deref(), new.role, None)
    }

    /// Add an account with an existing PHC hash (for imports).
    pub fn create_with_hash(
        &self,
        username: &str,
        hash: &str,
        role: Role,
    ) -> Result<User, UserError> {
        if !valid_username(username) {
            return Err(UserError::Invalid(format!(
                "{username:?} is not a valid user name"
            )));
        }
        if PasswordHash::new(hash).is_err() {
            return Err(UserError::Invalid(format!(
                "{username}: not a password hash"
            )));
        }
        self.insert(username, None, Some(hash), role, None)
    }

    fn insert(
        &self,
        username: &str,
        email: Option<&str>,
        hash: Option<&str>,
        role: Role,
        oidc: Option<(&str, &str)>,
    ) -> Result<User, UserError> {
        let id = {
            let conn = self.conn.lock();
            conn.execute(
                "INSERT INTO users (username, email, password_hash, role, oidc_issuer, oidc_subject, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![username, email, hash, role.name(), oidc.map(|o| o.0), oidc.map(|o| o.1), now_iso()],
            )?;
            conn.last_insert_rowid()
        };
        self.get(id)?.ok_or(UserError::NotFound)
    }

    /// Apply changes in one transaction; demoting the last administrator is refused. A new
    /// password ends the account's sessions.
    pub fn update(&self, id: i64, change: UserUpdate<'_>) -> Result<User, UserError> {
        let email = match change.email {
            Some(e) => Some(normalize_email(e)?),
            None => None,
        };
        let hash = match change.password {
            Some(p) => {
                check_password(p)?;
                Some(hash_password(p).map_err(|e| UserError::Db(e.to_string()))?)
            }
            None => None,
        };
        {
            let mut conn = self.conn.lock();
            // IMMEDIATE takes the write lock before the admin count is read, so two processes
            // cannot each remove "the other" administrator.
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let role: Option<String> = tx
                .query_row("SELECT role FROM users WHERE id = ?1", [id], |r| r.get(0))
                .optional()?;
            let Some(role) = role else {
                return Err(UserError::NotFound);
            };
            if let Some(new_role) = change.role {
                if role == "admin" && new_role != Role::Admin {
                    let admins: i64 =
                        tx.query_row("SELECT count(*) FROM users WHERE role = 'admin'", [], |r| {
                            r.get(0)
                        })?;
                    if admins <= 1 {
                        return Err(UserError::LastAdmin);
                    }
                }
                tx.execute(
                    "UPDATE users SET role = ?2 WHERE id = ?1",
                    params![id, new_role.name()],
                )?;
            }
            if let Some(email) = &email {
                tx.execute(
                    "UPDATE users SET email = ?2 WHERE id = ?1",
                    params![id, email],
                )?;
            }
            if let Some(hash) = &hash {
                tx.execute(
                    "UPDATE users SET password_hash = ?2, session_version = session_version + 1 WHERE id = ?1",
                    params![id, hash],
                )?;
            }
            tx.commit()?;
        }
        self.get(id)?.ok_or(UserError::NotFound)
    }

    /// Delete an account; the last administrator cannot be deleted.
    pub fn delete(&self, id: i64) -> Result<(), UserError> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let role: Option<String> = tx
            .query_row("SELECT role FROM users WHERE id = ?1", [id], |r| r.get(0))
            .optional()?;
        match role.as_deref() {
            None => return Err(UserError::NotFound),
            Some("admin") => {
                let admins: i64 =
                    tx.query_row("SELECT count(*) FROM users WHERE role = 'admin'", [], |r| {
                        r.get(0)
                    })?;
                if admins <= 1 {
                    return Err(UserError::LastAdmin);
                }
            }
            Some(_) => {}
        }
        tx.execute("DELETE FROM users WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(())
    }

    /// Remove the linked identity; the next OIDC sign-in of that identity no longer reaches this
    /// account. Ends the account's sessions.
    pub fn unlink_oidc(&self, id: i64) -> Result<User, UserError> {
        let changed = self.conn.lock().execute(
            "UPDATE users SET oidc_issuer = NULL, oidc_subject = NULL, session_version = session_version + 1 WHERE id = ?1",
            [id],
        )?;
        if changed == 0 {
            return Err(UserError::NotFound);
        }
        self.get(id)?.ok_or(UserError::NotFound)
    }

    pub fn touch_login(&self, id: i64) {
        if let Err(e) = self.conn.lock().execute(
            "UPDATE users SET last_login_at = ?2 WHERE id = ?1",
            params![id, now_iso()],
        ) {
            log::warn!("users: cannot record sign-in: {e}");
        }
    }

    /// Find or create the account for an OpenID Connect identity:
    /// 1. the account linked to this issuer and subject;
    /// 2. else, with `link_by_email` and an address the provider marks verified, an account with
    ///    that address and no linked identity, which gets linked (opt-in: it trusts the provider
    ///    to verify addresses and never to reassign them);
    /// 3. else, with `create`, a new account (user name from the claim, made unique).
    ///
    /// `provider_role` (roles from groups) is written to the account at every sign-in, also when
    /// that demotes the last administrator: with provider roles the provider decides, and its
    /// groups can promote someone again. New accounts without it get the user role.
    pub fn oidc_sign_in(
        &self,
        claims: &OidcClaims<'_>,
        provider_role: Option<Role>,
        link_by_email: bool,
        create: bool,
    ) -> Result<OidcOutcome, UserError> {
        let linked = {
            let conn = self.conn.lock();
            let mut st = conn.prepare_cached(&format!(
                "SELECT {COLUMNS} FROM users WHERE oidc_issuer = ?1 AND oidc_subject = ?2"
            ))?;
            st.query_row([claims.issuer, claims.subject], user_from_row)
                .optional()?
        };
        let outcome = if let Some(u) = linked {
            OidcOutcome::Existing(u)
        } else if let Some(u) = link_by_email
            .then(|| self.link_by_email(claims))
            .transpose()?
            .flatten()
        {
            OidcOutcome::Linked(u)
        } else if create {
            OidcOutcome::Created(self.create_for_identity(claims, provider_role)?)
        } else {
            return Ok(OidcOutcome::NoAccount);
        };
        let Some(role) = provider_role else {
            return Ok(outcome);
        };
        let set = |u: User| -> Result<User, UserError> {
            if u.role == role {
                return Ok(u);
            }
            self.conn.lock().execute(
                "UPDATE users SET role = ?2 WHERE id = ?1",
                params![u.id, role.name()],
            )?;
            self.get(u.id)?.ok_or(UserError::NotFound)
        };
        Ok(match outcome {
            OidcOutcome::Existing(u) => OidcOutcome::Existing(set(u)?),
            OidcOutcome::Linked(u) => OidcOutcome::Linked(set(u)?),
            other => other,
        })
    }

    /// Link an identity to a signed-in account (explicit linking). Fails if the identity
    /// already belongs to another account.
    pub fn link_oidc(&self, id: i64, issuer: &str, subject: &str) -> Result<User, UserError> {
        let changed = self.conn.lock().execute(
            "UPDATE users SET oidc_issuer = ?2, oidc_subject = ?3 WHERE id = ?1",
            params![id, issuer, subject],
        )?;
        if changed == 0 {
            return Err(UserError::NotFound);
        }
        self.get(id)?.ok_or(UserError::NotFound)
    }

    fn link_by_email(&self, claims: &OidcClaims<'_>) -> Result<Option<User>, UserError> {
        let Some(email) = claims.email.filter(|_| claims.email_verified) else {
            return Ok(None);
        };
        let changed = self.conn.lock().execute(
            "UPDATE users SET oidc_issuer = ?1, oidc_subject = ?2 \
             WHERE email = ?3 AND oidc_issuer IS NULL",
            params![claims.issuer, claims.subject, email],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        let conn = self.conn.lock();
        let mut st = conn.prepare_cached(&format!(
            "SELECT {COLUMNS} FROM users WHERE oidc_issuer = ?1 AND oidc_subject = ?2"
        ))?;
        Ok(st
            .query_row([claims.issuer, claims.subject], user_from_row)
            .optional()?)
    }

    fn create_for_identity(
        &self,
        claims: &OidcClaims<'_>,
        provider_role: Option<Role>,
    ) -> Result<User, UserError> {
        let base: String = claims
            .username
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || "._@-".contains(c) {
                    c
                } else {
                    '-'
                }
            })
            .take(56)
            .collect();
        let base = if valid_username(&base) {
            base
        } else {
            "user".to_string()
        };
        // Verified addresses only, and only if no other account uses it.
        let email = claims
            .email
            .filter(|e| claims.email_verified && valid_email(e))
            .filter(|e| {
                self.conn
                    .lock()
                    .query_row("SELECT 1 FROM users WHERE email = ?1", [e], |_| Ok(()))
                    .optional()
                    .is_ok_and(|r| r.is_none())
            });
        let role = provider_role.unwrap_or(Role::User);
        for n in 1..=100 {
            let name = if n == 1 {
                base.clone()
            } else {
                format!("{base}-{n}")
            };
            match self.insert(
                &name,
                email,
                None,
                role,
                Some((claims.issuer, claims.subject)),
            ) {
                Err(UserError::Conflict(m)) if m.contains("user name") => continue,
                other => return other,
            }
        }
        Err(UserError::Conflict(
            "no free user name for this identity".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> (tempfile::TempDir, UserDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = UserDb::open(dir.path()).unwrap();
        (dir, db)
    }

    fn add(db: &UserDb, name: &str, email: Option<&str>, role: Role) -> User {
        db.create(NewUser {
            username: name,
            email,
            password: Some("long enough"),
            role,
        })
        .unwrap()
    }

    #[test]
    fn accounts_are_validated_unique_and_never_expose_hashes() {
        let (dir, db) = db();
        let mode = std::fs::metadata(dir.path().join(DB_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o660);
        let ana = add(&db, "ana", Some("Ana@Example.net"), Role::Admin);
        assert!(ana.has_password && ana.oidc.is_none());
        let json = serde_json::to_string(&ana).unwrap();
        assert!(
            !json.contains("argon2") && !json.contains("session_version"),
            "{json}"
        );
        for (name, email, pw, expect) in [
            ("ANA", None, Some("long enough"), "user name is taken"),
            ("bo", Some("ana@example.NET"), None, "another user"),
            ("-bo", None, None, "user names are"),
            ("bo", Some("not-an-address"), None, "not an e-mail"),
            ("bo", None, Some("short"), "at least 8"),
        ] {
            let err = db
                .create(NewUser {
                    username: name,
                    email,
                    password: pw,
                    role: Role::User,
                })
                .unwrap_err()
                .to_string();
            assert!(err.contains(expect), "{name}: {err}");
        }
        assert!(
            db.create(NewUser {
                username: "oidc.only",
                email: None,
                password: None,
                role: Role::User
            })
            .is_ok()
        );
        assert_eq!(db.list().unwrap().len(), 2);
    }

    #[test]
    fn passwords_verify_and_changes_end_sessions() {
        let (_dir, db) = db();
        let ana = add(&db, "ana", None, Role::Admin);
        assert_eq!(
            db.verify_password("ana", "long enough")
                .unwrap()
                .map(|u| u.id),
            Some(ana.id)
        );
        assert_eq!(
            db.verify_password("ANA", "long enough")
                .unwrap()
                .map(|u| u.id),
            Some(ana.id)
        );
        assert!(db.verify_password("ana", "wrong one").unwrap().is_none());
        assert!(
            db.verify_password("nobody", "long enough")
                .unwrap()
                .is_none()
        );
        db.create(NewUser {
            username: "nopw",
            email: None,
            password: None,
            role: Role::User,
        })
        .unwrap();
        assert!(db.verify_password("nopw", "").unwrap().is_none());

        let changed = db
            .update(
                ana.id,
                UserUpdate {
                    password: Some("another secret"),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(changed.session_version > ana.session_version);
        assert!(
            db.verify_password("ana", "long enough").unwrap().is_none(),
            "old password, even cached"
        );
        assert!(
            db.verify_password("ana", "another secret")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn the_last_administrator_cannot_be_demoted_or_deleted() {
        let (_dir, db) = db();
        let ana = add(&db, "ana", None, Role::Admin);
        let bo = add(&db, "bo", None, Role::User);
        let demote = UserUpdate {
            role: Some(Role::User),
            ..Default::default()
        };
        assert_eq!(db.update(ana.id, demote).unwrap_err(), UserError::LastAdmin);
        assert_eq!(db.delete(ana.id).unwrap_err(), UserError::LastAdmin);
        db.update(
            bo.id,
            UserUpdate {
                role: Some(Role::Admin),
                ..Default::default()
            },
        )
        .unwrap();
        db.delete(ana.id).unwrap();
        assert_eq!(db.count_admins().unwrap(), 1);
        assert_eq!(db.delete(ana.id).unwrap_err(), UserError::NotFound);
    }

    #[test]
    fn oidc_identities_link_by_subject_or_verified_email_only() {
        let (_dir, db) = db();
        let ana = add(&db, "ana", Some("ana@example.net"), Role::Admin);
        let claims = |sub: &'static str,
                      name: &'static str,
                      email: Option<&'static str>,
                      verified: bool| OidcClaims {
            issuer: "https://id.example.net",
            subject: sub,
            username: name,
            email,
            email_verified: verified,
        };
        // Matching verified address without link_by_email: a separate, ordinary account.
        let OidcOutcome::Created(other) = db
            .oidc_sign_in(
                &claims("s9", "ana", Some("ana@example.net"), true),
                None,
                false,
                true,
            )
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(
            (other.username.as_str(), other.email.as_deref(), other.role),
            ("ana-2", None, Role::User)
        );
        db.delete(other.id).unwrap();
        // Unverified address, even with link_by_email: no link.
        let OidcOutcome::Created(other) = db
            .oidc_sign_in(
                &claims("s0", "ana", Some("ana@example.net"), false),
                None,
                true,
                true,
            )
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(
            (other.username.as_str(), other.email.as_deref(), other.role),
            ("ana-2", None, Role::User)
        );
        assert!(
            other.id > ana.id + 1,
            "ids of deleted accounts are not reused"
        );
        // Verified address with link_by_email: the existing account is linked and keeps its role.
        let OidcOutcome::Linked(linked) = db
            .oidc_sign_in(
                &claims("s1", "whatever", Some("ANA@example.net"), true),
                None,
                true,
                true,
            )
            .unwrap()
        else {
            panic!()
        };
        assert_eq!((linked.id, linked.role), (ana.id, Role::Admin));
        // Next time by subject, even with another address; the provider's role is applied.
        let again = db.oidc_sign_in(
            &claims("s1", "x", Some("new@example.net"), true),
            Some(Role::User),
            true,
            true,
        );
        assert!(
            matches!(again, Ok(OidcOutcome::Existing(u)) if u.id == ana.id && u.role == Role::User)
        );
        // Already linked accounts are not re-linked by address.
        assert!(matches!(
            db.oidc_sign_in(
                &claims("s2", "eve", Some("ana@example.net"), true),
                None,
                true,
                false
            ),
            Ok(OidcOutcome::NoAccount)
        ));
        db.unlink_oidc(ana.id).unwrap();
        assert!(matches!(
            db.oidc_sign_in(&claims("s1", "x", None, false), None, true, false),
            Ok(OidcOutcome::NoAccount)
        ));
        // Explicit linking by a signed-in account; an identity belongs to one account only.
        let bo = add(&db, "bo", None, Role::User);
        assert_eq!(
            db.link_oidc(ana.id, "https://id.example.net", "s1")
                .unwrap()
                .oidc
                .unwrap()
                .subject,
            "s1"
        );
        assert!(matches!(
            db.link_oidc(bo.id, "https://id.example.net", "s1"),
            Err(UserError::Conflict(_))
        ));
    }

    #[test]
    fn a_second_process_sees_changes_and_cannot_remove_the_last_admin_concurrently() {
        let (dir, db) = db();
        let other = UserDb::open(dir.path()).unwrap();
        let bo = add(&other, "bo", None, Role::User);
        assert_eq!(
            db.get(bo.id).unwrap().map(|u| u.username),
            Some("bo".into())
        );
        other.delete(bo.id).unwrap();
        assert!(db.get(bo.id).unwrap().is_none());

        let a = add(&db, "a", None, Role::Admin);
        let b = add(&db, "b", None, Role::Admin);
        let other = std::sync::Arc::new(other);
        let o = other.clone();
        let t = std::thread::spawn(move || o.delete(a.id));
        let mine = db.delete(b.id);
        let theirs = t.join().unwrap();
        assert_eq!(
            [mine.is_ok(), theirs.is_ok()]
                .iter()
                .filter(|ok| **ok)
                .count(),
            1,
            "exactly one of two concurrent deletions succeeds: {mine:?} {theirs:?}"
        );
        assert_eq!(db.count_admins().unwrap(), 1);
    }
}
