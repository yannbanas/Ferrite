//! Authentication.
//!
//! v1 uses `AuthenticationCleartextPassword`, which is safe here only
//! because the listener requires TLS by default (see [`crate::tls`]): the
//! password never crosses the network in the clear. Server-side the
//! password is never stored in the clear either — accounts hold a salted
//! SHA-256 verifier compared in constant time.
//!
//! Known gap, deliberately deferred rather than blocking v1:
//! **SCRAM-SHA-256** (RFC 7677), which modern PostgreSQL uses by default,
//! is not implemented. It removes the need for the client to reveal the
//! password at all and binds the exchange to the TLS channel. The message
//! plumbing for it already exists (`AuthenticationSASL` is just another
//! `R` message), so adding it is a self-contained follow-up. A salted
//! SHA-256 verifier is also not a password KDF: Argon2id should replace it
//! at the same time.

use std::collections::HashMap;

use async_trait::async_trait;
use ferrite_common::{Identity, Permission, Role};
use rand::RngCore;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::error::ProtocolError;

/// Who the client turned out to be, once authenticated.
#[derive(Debug, Clone, PartialEq)]
pub struct AuthOutcome {
    pub identity: Identity,
    pub role: Role,
}

/// Verifies a user's credentials.
///
/// Async because a real implementation reads accounts out of the system
/// catalog. Implementations must not leak, through timing or through
/// distinct error messages, whether the user exists — see
/// [`StaticAuthenticator`] for the reference behaviour.
#[async_trait]
pub trait Authenticator: Send + Sync + 'static {
    async fn authenticate(
        &self,
        user: &str,
        database: &str,
        password: &[u8],
    ) -> Result<AuthOutcome, ProtocolError>;
}

/// A salted SHA-256 password verifier.
#[derive(Clone)]
pub struct PasswordVerifier {
    salt: [u8; 16],
    digest: [u8; 32],
}

impl PasswordVerifier {
    /// Builds a verifier for `password` with a fresh random salt.
    pub fn new(password: &str) -> Self {
        let mut salt = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut salt);
        let digest = derive(&salt, password.as_bytes());
        Self { salt, digest }
    }

    pub fn from_parts(salt: [u8; 16], digest: [u8; 32]) -> Self {
        Self { salt, digest }
    }

    /// Constant-time comparison. Both sides are fixed-width digests, so
    /// neither the password's content nor its length is exposed by the
    /// comparison itself.
    pub fn verify(&self, candidate: &[u8]) -> bool {
        derive(&self.salt, candidate).ct_eq(&self.digest).into()
    }
}

impl std::fmt::Debug for PasswordVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PasswordVerifier(redacted)")
    }
}

fn derive(salt: &[u8; 16], password: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"ferrite-password-v1");
    hasher.update(salt);
    hasher.update(password);
    hasher.finalize().into()
}

#[derive(Debug, Clone)]
struct Account {
    identity: Identity,
    role: Role,
    verifier: PasswordVerifier,
}

/// An in-memory account table.
///
/// Intended for bootstrapping and tests: the real deployment path is an
/// [`Authenticator`] backed by the system catalog once `ferrite-catalog`
/// grows a roles table.
pub struct StaticAuthenticator {
    accounts: HashMap<String, Account>,
    /// Compared against when the user does not exist, so a failed lookup
    /// costs the same as a wrong password and cannot be used to enumerate
    /// user names.
    decoy: PasswordVerifier,
}

impl Default for StaticAuthenticator {
    fn default() -> Self {
        Self::new()
    }
}

impl StaticAuthenticator {
    pub fn new() -> Self {
        Self {
            accounts: HashMap::new(),
            decoy: PasswordVerifier::new("ferrite-decoy"),
        }
    }

    /// Adds an account whose [`Identity`] is derived deterministically from
    /// the user name. A production deployment derives the identity from the
    /// account's public key instead.
    pub fn with_user(self, user: &str, password: &str, role: Role) -> Self {
        let identity = identity_for_user(user);
        self.with_account(user, password, identity, role)
    }

    pub fn with_account(
        mut self,
        user: &str,
        password: &str,
        identity: Identity,
        role: Role,
    ) -> Self {
        self.accounts.insert(
            user.to_owned(),
            Account {
                identity,
                role,
                verifier: PasswordVerifier::new(password),
            },
        );
        self
    }
}

/// Stable identity for a password account: SHA-256 over a domain-separated
/// user name. Deterministic so restarts do not change who a user is.
pub fn identity_for_user(user: &str) -> Identity {
    let mut hasher = Sha256::new();
    hasher.update(b"ferrite-identity-v1");
    hasher.update(user.as_bytes());
    Identity(hasher.finalize().into())
}

#[async_trait]
impl Authenticator for StaticAuthenticator {
    async fn authenticate(
        &self,
        user: &str,
        _database: &str,
        password: &[u8],
    ) -> Result<AuthOutcome, ProtocolError> {
        let account = self.accounts.get(user);
        let verifier = account.map(|a| &a.verifier).unwrap_or(&self.decoy);
        let ok = verifier.verify(password);
        match account {
            Some(account) if ok => Ok(AuthOutcome {
                identity: account.identity,
                role: account.role.clone(),
            }),
            _ => Err(ProtocolError::AuthFailed(user.to_owned())),
        }
    }
}

/// A role that may connect and read, which is the least-privilege default
/// for an interactive client.
pub fn read_only_role() -> Role {
    Role {
        name: "readonly".to_owned(),
        permissions: vec![Permission::Connect, Permission::Select],
    }
}

/// A role with every permission. Only for local bootstrapping.
pub fn superuser_role() -> Role {
    Role {
        name: "superuser".to_owned(),
        permissions: vec![
            Permission::Connect,
            Permission::CreateTable,
            Permission::Select,
            Permission::Insert,
            Permission::Update,
            Permission::Delete,
            Permission::Execute,
            Permission::Admin,
        ],
    }
}

/// Deny-by-default connection check: a role must carry `Connect` (or
/// `Admin`) to open a session at all.
pub fn may_connect(role: &Role) -> bool {
    role.permissions
        .iter()
        .any(|p| matches!(p, Permission::Connect | Permission::Admin))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_verifier_accepts_only_the_right_password() {
        let v = PasswordVerifier::new("hunter2");
        assert!(v.verify(b"hunter2"));
        assert!(!v.verify(b"hunter3"));
        assert!(!v.verify(b""));
        assert!(!v.verify(b"hunter2\0"));
    }

    #[test]
    fn two_verifiers_for_the_same_password_differ_by_salt() {
        let a = PasswordVerifier::new("same");
        let b = PasswordVerifier::new("same");
        assert_ne!(a.digest, b.digest);
        assert!(a.verify(b"same") && b.verify(b"same"));
    }

    #[test]
    fn the_verifier_never_prints_its_secret() {
        let v = PasswordVerifier::new("hunter2");
        assert_eq!(format!("{v:?}"), "PasswordVerifier(redacted)");
    }

    #[tokio::test]
    async fn unknown_users_and_wrong_passwords_fail_the_same_way() {
        let auth = StaticAuthenticator::new().with_user("ferrite", "hunter2", superuser_role());
        let wrong = auth.authenticate("ferrite", "app", b"nope").await;
        let missing = auth.authenticate("ghost", "app", b"hunter2").await;
        assert!(matches!(wrong, Err(ProtocolError::AuthFailed(_))));
        assert!(matches!(missing, Err(ProtocolError::AuthFailed(_))));
    }

    #[tokio::test]
    async fn a_valid_login_yields_a_stable_identity() {
        let auth = StaticAuthenticator::new().with_user("ferrite", "hunter2", read_only_role());
        let outcome = auth
            .authenticate("ferrite", "app", b"hunter2")
            .await
            .unwrap();
        assert_eq!(outcome.identity, identity_for_user("ferrite"));
        assert_eq!(outcome.role.name, "readonly");
    }

    #[test]
    fn connecting_is_denied_unless_the_role_grants_it() {
        assert!(may_connect(&read_only_role()));
        assert!(may_connect(&superuser_role()));
        assert!(!may_connect(&Role {
            name: "nologin".into(),
            permissions: vec![Permission::Select],
        }));
    }
}
