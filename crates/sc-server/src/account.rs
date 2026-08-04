//! Public accounts: a filer signs in with a magic link and files under it.
//!
//! ## Why an account rather than verifying each request
//!
//! An earlier design emailed a verification link per *request*. That made every
//! filing an unauthenticated mail send — structurally an open relay — and left
//! rate limiting nothing trustworthy to key on, since an email address is chosen
//! by whoever is typing.
//!
//! An account moves the mail to **once per person**. What follows is
//! authenticated, so the budget keys on something the filer cannot mint more of,
//! and abuse becomes revocable rather than merely rate-limited.
//!
//! ## Stored hashed, like everything else here
//!
//! Session tokens are kept as SHA-256, never in the clear — the same rule
//! [`crate::auth`] applies to device tokens, for the same reason: the data volume
//! is backed up and copied around.
//!
//! Email addresses are hashed too, with a display hint kept alongside. **This is
//! not anonymisation** — the space of real email addresses is small enough to
//! brute-force against a list, and anyone claiming otherwise is overselling it.
//! What it buys is narrower and still worth having: a copied volume is not a
//! mailing list, and a casual look at the file discloses nobody.
//!
//! ## Separate from `credentials.json`
//!
//! [`Credentials`](crate::auth::Credentials) is read on **every** request.
//! Accounts are self-serve and therefore unbounded, so putting them there would
//! let a stranger decide how much JSON the hot path parses on every hit. They
//! live in their own file, read only on the paths that need them.

use serde::{Deserialize, Serialize};

use crate::auth::{hash, matches, mint_secret};

/// How long a magic link is good for.
///
/// Long enough to switch to a phone and open mail; short enough that a link
/// sitting in a mailbox archive is dead. The enrolment code has no expiry at
/// all — that gap is deliberately not repeated here, because this one travels
/// through other people's infrastructure.
pub const LINK_TTL_MS: u64 = 15 * 60 * 1000;

/// How long a consumed or expired link is kept before sweeping.
///
/// Kept rather than deleted immediately so a second click can say "already used"
/// instead of "invalid link" — see [`Links::consume`].
pub const LINK_RETAIN_MS: u64 = 24 * 60 * 60 * 1000;

/// A person who can file requests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    /// Random, not derived from the email — an id that encodes the address would
    /// undo the hashing it sits next to.
    pub id: String,
    /// SHA-256 of the normalised address. **Never the address.**
    pub email_hash: String,
    /// `j***@example.com`, for the revoke list. Enough to recognise an account
    /// you meant to revoke, not enough to be a contact list.
    pub email_hint: String,
    pub created_ms: u64,
    /// Revoked accounts are kept, not deleted — the same reasoning as a revoked
    /// device: a list that silently shrinks cannot answer "did I already deal
    /// with that?".
    #[serde(default)]
    pub revoked: bool,
    /// The username this account signs in with, lowercased.
    ///
    /// **Not a permission.** It says who somebody proved they are, and nothing
    /// about what they may see — whether this login is the *administrator* comes
    /// from `admin.json` and whether it is an *owner* comes from the roster,
    /// both checked on every request. So revoking either demotes them
    /// immediately, and this field is left alone: it remains a true statement
    /// about how they signed in.
    ///
    /// `None` for an account created by a magic link, which is every filer.
    /// Filers are strangers and should not be made to keep a credential; the two
    /// named roles are people who come back.
    ///
    /// **Unique across live accounts**, enforced by [`Accounts::create_login`].
    /// With GitHub a login was proved by a third party and could not collide;
    /// now two accounts sharing one would make `Admin::is` match whichever came
    /// first, which is not a coin toss anybody should be running.
    #[serde(default)]
    pub login: Option<String>,
    /// argon2id of the password, when this account has one.
    ///
    /// **[`crate::auth::hash_password`], never [`crate::auth::hash`]** — see that
    /// module for why the two are not interchangeable.
    #[serde(default)]
    pub password_hash: Option<String>,
    /// How many wrong passwords in a row.
    ///
    /// Reset by a correct one. Feeds [`Account::retry_at`], and exists because
    /// the rate limiter alone allows ~29,000 guesses a day against a known
    /// username — GitHub used to do this throttling for us.
    #[serde(default)]
    pub failed_attempts: u32,
    /// Unix ms before which another attempt is refused **without checking the
    /// password**.
    ///
    /// Checked before the hash is computed, so a locked-out account is not also
    /// a way to spend this server's CPU.
    #[serde(default)]
    pub next_attempt_ms: u64,
}

/// The longest a wrong password makes somebody wait.
///
/// **A delay, not a lockout.** With no second way in, locking the administrator
/// out means editing `admin.json` on the volume — and anybody who knows the
/// username could trigger that deliberately. A ceiling of five minutes costs a
/// guesser everything (it caps them at a few hundred attempts a day rather than
/// tens of thousands) and costs the rightful holder a short wait.
pub const MAX_BACKOFF_MS: u64 = 5 * 60 * 1000;

/// How long to wait after `n` consecutive failures.
///
/// The first two are free: a typo should not be punished, and the point is to
/// make *sustained* guessing expensive rather than to make one mistake annoying.
/// After that it doubles — 2s, 4s, 8s — to the ceiling.
pub fn backoff_ms(failures: u32) -> u64 {
    match failures {
        0..=2 => 0,
        n => MAX_BACKOFF_MS.min(1_000u64.saturating_mul(1 << (n - 2).min(20))),
    }
}

impl Account {
    /// When may this account try a password again?
    pub fn retry_at(&self) -> u64 {
        self.next_attempt_ms
    }

    /// Record a wrong password, and return when they may try again.
    pub fn note_failure(&mut self, now_ms: u64) -> u64 {
        self.failed_attempts = self.failed_attempts.saturating_add(1);
        self.next_attempt_ms = now_ms.saturating_add(backoff_ms(self.failed_attempts));
        self.next_attempt_ms
    }

    /// Record a correct password. Clears the backoff entirely.
    pub fn note_success(&mut self) {
        self.failed_attempts = 0;
        self.next_attempt_ms = 0;
    }
}

/// A signed-in browser.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub account_id: String,
    /// SHA-256 of the cookie token. **Never the token.**
    pub token_hash: String,
    pub issued_ms: u64,
    #[serde(default)]
    pub revoked: bool,
    /// Unix ms when this browser last proved itself with a credential.
    ///
    /// Equal to `issued_ms` on a session sign-in just opened, and moved forward
    /// when a re-authentication lands on it.
    ///
    /// **Per session, not per account.** Proving it again on a laptop must not
    /// privilege a phone that has been sitting signed in for a month — that is
    /// the entire meaning of proving it again, and an account-level stamp would
    /// quietly grant it everywhere.
    ///
    /// `#[serde(default)]` reads as 0 on a session written before this existed,
    /// which [`Session::fresh`] treats as stale. An upgrade therefore asks for
    /// the hop rather than honouring an old session as freshly proved — the safe
    /// direction for that default to fall.
    #[serde(default)]
    pub authed_ms: u64,
}

/// How recently a browser must have proved itself to change a secret.
///
/// Five minutes, not ten and not an hour. Typing a password takes seconds, so
/// the window only has to cover reaching for it and typing it. Long enough that
/// changing two settings does not mean proving it twice; short enough that a
/// laptop walked away from is not a standing key to the secrets.
///
/// **The number did not change when the mechanism did**, and that is not
/// inertia: what it measures is how long a browser may coast on a proof, which
/// has nothing to do with how the proof was made. A hop to a third party and a
/// password field both take about as long, so both fit the same window.
pub const FRESH_AUTH_MS: u64 = 5 * 60 * 1000;

impl Session {
    /// Has this browser proved itself recently enough to change a secret?
    pub fn fresh(&self, now_ms: u64) -> bool {
        // `saturating_sub` for the same reason every other expiry here uses it:
        // a clock stepping backwards must not make an old session read as
        // freshly proved.
        now_ms.saturating_sub(self.authed_ms) <= FRESH_AUTH_MS
    }
}

/// The accounts, and the file state they were read from.
///
/// **Why this exists now and did not before.** `identify` used to check the
/// small, developer-sized credentials file first and read this one lazily, only
/// where a public surface existed. That ordering was the defence: a stranger
/// could not make an unauthenticated request cost a parse of an attacker-sized
/// file.
///
/// Giving the administrator an account removed it. This is the only
/// credential store left, so every cookie-bearing request reaches it — including
/// one carrying a cookie that matches nothing, which is what a guesser sends,
/// and which is resolved *before* the rate limiter runs.
///
/// So the same mtime-keyed cache the roster uses: a `stat` on every request and
/// a parse only when the file has actually changed. `max_accounts` still bounds
/// how large it can get; this bounds how often anyone pays for it.
#[derive(Debug, Default)]
pub struct AccountsCache {
    mtime: Option<std::time::SystemTime>,
    value: std::sync::Arc<Accounts>,
}

impl AccountsCache {
    /// The accounts as they are on disk right now.
    ///
    /// A failed read yields whatever was last good rather than an empty set: a
    /// transient error must not sign everybody out at once, which would look
    /// exactly like a revocation nobody performed.
    pub fn current(&mut self, path: &std::path::Path) -> std::sync::Arc<Accounts> {
        let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        if mtime != self.mtime {
            if let Ok(text) = std::fs::read_to_string(path) {
                if let Ok(parsed) = serde_json::from_str::<Accounts>(&text) {
                    self.value = std::sync::Arc::new(parsed);
                }
            } else if mtime.is_none() {
                // No file yet — a fresh install, where an empty set is the right
                // answer rather than a stale one.
                self.value = std::sync::Arc::new(Accounts::default());
            }
            self.mtime = mtime;
        }
        std::sync::Arc::clone(&self.value)
    }

    /// Forget what was read, so the next look re-parses.
    ///
    /// Called after every write. **Revocation is the reason this is not left to
    /// the mtime alone**: a filesystem with coarse timestamps can record a write
    /// inside the same tick as the read before it, and a developer who revokes
    /// somebody and watches them keep filing would reasonably conclude it had
    /// not worked.
    pub fn invalidate(&mut self) {
        self.mtime = None;
    }
}

/// An outstanding sign-in link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MagicLink {
    /// SHA-256 of the emailed token. **Never the token.**
    pub token_hash: String,
    pub email_hash: String,
    pub email_hint: String,
    pub issued_ms: u64,
    /// Spent, but kept until swept so a second click can be told apart from a
    /// fabricated token.
    #[serde(default)]
    pub consumed: bool,
}

impl MagicLink {
    pub fn expired(&self, now_ms: u64) -> bool {
        // `saturating_sub` so a clock that steps backwards cannot mint a link
        // valid for the next forty-nine days.
        now_ms.saturating_sub(self.issued_ms) > LINK_TTL_MS
    }

    pub fn usable(&self, now_ms: u64) -> bool {
        !self.consumed && !self.expired(now_ms)
    }
}

/// Why a link could not be spent.
///
/// `AlreadyUsed` is distinguished from `Invalid` **only** so the page can say
/// something true to a person who double-clicked — "you are probably already
/// signed in" rather than "invalid link", which reads as a bug to someone whose
/// sign-in just worked. It leaks that a token once existed, which is worth it
/// because the token was theirs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkError {
    /// Unknown, expired, or fabricated. Deliberately one variant: distinguishing
    /// "expired" from "never existed" would confirm a guessed token.
    Invalid,
    AlreadyUsed,
}

/// Outstanding sign-in links.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Links {
    #[serde(default)]
    pub links: Vec<MagicLink>,
}

impl Links {
    /// Mint a link for an address. Returns the token to email.
    ///
    /// The token is returned **once** and never stored — only its hash is kept,
    /// so a leaked volume grants nobody a sign-in.
    pub fn issue(&mut self, email_hash: &str, email_hint: &str, now_ms: u64) -> String {
        let token = mint_secret();
        self.links.push(MagicLink {
            token_hash: hash(&token),
            email_hash: email_hash.to_string(),
            email_hint: email_hint.to_string(),
            issued_ms: now_ms,
            consumed: false,
        });
        token
    }

    /// Is there a usable link for this token? **Changes nothing.**
    ///
    /// What the landing page calls. Mail scanners prefetch links — Outlook Safe
    /// Links, Proofpoint and friends issue a GET on every URL in a message — so a
    /// GET that consumed the token would burn it before the human ever saw the
    /// mail.
    pub fn peek(&self, token: &str, now_ms: u64) -> Option<&MagicLink> {
        self.links
            .iter()
            .find(|l| matches(token, &l.token_hash) && l.usable(now_ms))
    }

    /// Spend a link, returning the address it was issued for.
    pub fn consume(
        &mut self,
        token: &str,
        now_ms: u64,
    ) -> std::result::Result<(String, String), LinkError> {
        let Some(link) = self
            .links
            .iter_mut()
            .find(|l| matches(token, &l.token_hash))
        else {
            return Err(LinkError::Invalid);
        };
        if link.consumed {
            return Err(LinkError::AlreadyUsed);
        }
        if link.expired(now_ms) {
            return Err(LinkError::Invalid);
        }
        link.consumed = true;
        Ok((link.email_hash.clone(), link.email_hint.clone()))
    }

    /// How many links are outstanding and still usable.
    ///
    /// The cap on this is what actually bounds mail sending: the rate limiter
    /// shapes traffic, but 30 a minute sustained is tens of thousands a day.
    pub fn outstanding(&self, now_ms: u64) -> usize {
        self.links.iter().filter(|l| l.usable(now_ms)).count()
    }

    /// Drop links that are long past being useful.
    pub fn sweep(&mut self, now_ms: u64) {
        self.links
            .retain(|l| now_ms.saturating_sub(l.issued_ms) < LINK_RETAIN_MS);
    }
}

/// The account store.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Accounts {
    #[serde(default)]
    pub accounts: Vec<Account>,
    #[serde(default)]
    pub sessions: Vec<Session>,
}

impl Accounts {
    /// Find a live account by hashed address.
    /// The live account for an address, mutably.
    pub fn by_email_mut(&mut self, email_hash: &str) -> Option<&mut Account> {
        self.accounts
            .iter_mut()
            .find(|a| !a.revoked && a.email_hash == email_hash)
    }

    pub fn by_email(&self, email_hash: &str) -> Option<&Account> {
        self.accounts
            .iter()
            .find(|a| !a.revoked && a.email_hash == email_hash)
    }

    /// Does an account exist for this address, revoked or not?
    ///
    /// Distinct from [`by_email`](Accounts::by_email): a revoked account must not
    /// be silently re-created by signing in again, or revocation would mean
    /// nothing.
    pub fn any_by_email(&self, email_hash: &str) -> Option<&Account> {
        self.accounts.iter().find(|a| a.email_hash == email_hash)
    }

    /// Create an account. Caller has already checked the address is not taken.
    pub fn create(&mut self, email_hash: &str, email_hint: &str, now_ms: u64) -> Account {
        let account = Account {
            // Random rather than time-derived: two signups in the same
            // millisecond would otherwise share an id, and revoking one would
            // revoke the other.
            id: format!("acct-{}", &mint_secret()[..16]),
            email_hash: email_hash.to_string(),
            email_hint: email_hint.to_string(),
            created_ms: now_ms,
            revoked: false,
            // A magic-link signup. `create_login` is the only other way in, and
            // it is the only way an account ever acquires a password.
            login: None,
            password_hash: None,
            failed_attempts: 0,
            next_attempt_ms: 0,
        };
        self.accounts.push(account.clone());
        account
    }

    /// The live account holding this username, if any.
    ///
    /// Lowercased on the way in, so a caller holding whatever casing somebody
    /// typed matches a record written from a different source — the same rule
    /// `Admin::is` and `Roster::owner_for` already keep.
    pub fn by_login(&self, login: &str) -> Option<&Account> {
        let login = login.trim().to_ascii_lowercase();
        self.accounts
            .iter()
            .find(|a| !a.revoked && a.login.as_deref() == Some(login.as_str()))
    }

    fn by_login_mut(&mut self, login: &str) -> Option<&mut Account> {
        let login = login.trim().to_ascii_lowercase();
        self.accounts
            .iter_mut()
            .find(|a| !a.revoked && a.login.as_deref() == Some(login.as_str()))
    }

    /// Create an account that signs in with a username and password.
    ///
    /// **Refuses a login already taken by a live account.** Nothing enforced
    /// this while logins came from GitHub, because a third party guaranteed
    /// uniqueness; now two accounts sharing one would make `Admin::is` match
    /// whichever the iteration reached first.
    ///
    /// The email hash is synthesised from the login rather than left empty: it
    /// is the store's other unique key, and two accounts colliding there would
    /// break `by_email` the same way.
    pub fn create_login(
        &mut self,
        login: &str,
        password: &str,
        now_ms: u64,
    ) -> std::result::Result<Account, String> {
        // **The login is an email address, and it keys the account.** It used
        // to be a separate name over a synthesised `hash("login:{name}")`, which
        // meant one person could hold two unrelated rows \u2014 a password account
        // and a magic-link account \u2014 that no code could ever reconcile.
        //
        // The consequence is worth stating: a magic link now reaches an
        // administrator's account exactly as it reaches a filer's, because there
        // is one row and two ways to prove you own the address. Whoever can read
        // that inbox can sign in as its owner. That is the price of one
        // identity, and it was chosen deliberately over two.
        if !valid_email(login) {
            return Err("that is not an email address".to_string());
        }
        let login = normalize_email(login);
        let email_hash = crate::auth::hash(&login);
        let password_hash = crate::auth::hash_password(password)?;

        // Adopt an existing row rather than making a second one: somebody who
        // filed a request last week and is made an owner today is the same
        // person, and two rows would split their history from their permissions.
        if let Some(existing) = self.by_email_mut(&email_hash) {
            if existing.password_hash.is_some() {
                return Err(format!("{login:?} is taken"));
            }
            existing.login = Some(login);
            existing.password_hash = Some(password_hash);
            return Ok(existing.clone());
        }

        let mut account = self.create(&email_hash, &email_hint(&login), now_ms);
        account.login = Some(login);
        account.password_hash = Some(password_hash);

        // `create` pushed a copy already; make the stored one match.
        let idx = self.accounts.len() - 1;
        self.accounts[idx] = account.clone();
        Ok(account)
    }

    /// Replace an account's password.
    ///
    /// Clears the backoff, because somebody who just proved themselves well
    /// enough to change it is not the guesser it exists to slow down.
    pub fn set_password(&mut self, login: &str, password: &str) -> std::result::Result<(), String> {
        let hash = crate::auth::hash_password(password)?;
        let Some(account) = self.by_login_mut(login) else {
            return Err("no such account".to_string());
        };
        account.password_hash = Some(hash);
        account.note_success();
        Ok(())
    }

    /// Check a password, recording the attempt.
    ///
    /// `Ok(account_id)` on success. `Err(retry_at_ms)` on failure — including
    /// when the account is still backing off, in which case **the password is
    /// not checked at all**, so a locked-out account cannot be used to spend
    /// this server's CPU.
    ///
    /// One answer for "no such account", "wrong password" and "still waiting":
    /// the caller renders the same page for all three, because distinguishing
    /// them tells a guesser which half they got right.
    pub fn check_password(
        &mut self,
        login: &str,
        password: &str,
        now_ms: u64,
    ) -> std::result::Result<String, u64> {
        let Some(account) = self.by_login_mut(login) else {
            return Err(0);
        };
        if now_ms < account.next_attempt_ms {
            return Err(account.next_attempt_ms);
        }
        let Some(stored) = account.password_hash.clone() else {
            return Err(0);
        };
        if crate::auth::password_matches(password, &stored) {
            account.note_success();
            Ok(account.id.clone())
        } else {
            Err(account.note_failure(now_ms))
        }
    }

    /// Open a session for an account, returning the cookie token.
    pub fn open_session(&mut self, account_id: &str, now_ms: u64) -> String {
        let token = mint_secret();
        self.sessions.push(Session {
            account_id: account_id.to_string(),
            token_hash: hash(&token),
            issued_ms: now_ms,
            revoked: false,
            // A session sign-in just opened *is* freshly proved: whoever holds
            // it typed the credential a moment ago.
            authed_ms: now_ms,
        });
        token
    }

    /// Which account holds this session token, if any live one does.
    ///
    /// Liveness is **derived**, not copied: a revoked account's sessions stop
    /// working without anyone having to walk and flip each one. Revocation that
    /// depends on remembering to update N other records is revocation that
    /// eventually misses one.
    pub fn session_for(&self, token: &str) -> Option<&Account> {
        let session = self
            .sessions
            .iter()
            .find(|s| !s.revoked && matches(token, &s.token_hash))?;
        self.accounts
            .iter()
            .find(|a| a.id == session.account_id && !a.revoked)
    }

    /// Was this token's session proved with a credential recently?
    ///
    /// Separate from [`session_for`](Accounts::session_for), which answers *who*
    /// — this answers *how recently*, and the two are wanted at different
    /// moments by different callers. Folding them together would make every
    /// route that only needs an identity carry a freshness it does not use.
    pub fn session_fresh(&self, token: &str, now_ms: u64) -> bool {
        self.sessions
            .iter()
            .find(|s| !s.revoked && matches(token, &s.token_hash))
            .is_some_and(|s| s.fresh(now_ms))
    }

    /// Record that this session has just proved itself again.
    ///
    /// `true` when a live session was found and stamped. Called by the OAuth
    /// callback when a re-authentication lands on a browser that already holds
    /// a session, so proving it again does not have to mean signing out first.
    pub fn refresh_session(&mut self, token: &str, now_ms: u64) -> bool {
        match self
            .sessions
            .iter_mut()
            .find(|s| !s.revoked && matches(token, &s.token_hash))
        {
            Some(s) => {
                s.authed_ms = now_ms;
                true
            }
            None => false,
        }
    }

    /// Revoke an account. `true` if one was live and now is not.
    pub fn revoke(&mut self, id: &str) -> bool {
        match self.accounts.iter_mut().find(|a| a.id == id && !a.revoked) {
            Some(a) => {
                a.revoked = true;
                true
            }
            None => false,
        }
    }

    /// Accounts that can still file.
    pub fn live(&self) -> Vec<&Account> {
        self.accounts.iter().filter(|a| !a.revoked).collect()
    }
}

/// Normalise an address before hashing.
///
/// Lowercase and trim, and **nothing else**. Stripping `+tags` or dots is
/// tempting — it stops one mailbox minting many accounts — but it is wrong on
/// providers where `a.b@` and `ab@` are different people, and the failure mode is
/// merging two humans into one account. The abuse it would prevent is already
/// handled by the outstanding-link cap and by revocation.
pub fn normalize_email(email: &str) -> String {
    email.trim().to_ascii_lowercase()
}

/// Is this structurally an email address?
///
/// Deliberately minimal. A regex attempting RFC 5321 is always subtly wrong, and
/// its failure mode is rejecting somebody's real address — which they cannot work
/// around. Delivery is the real validator: an address that does not exist never
/// receives the link.
pub fn valid_email(email: &str) -> bool {
    let e = email.trim();
    if e.len() > 254 || e.is_empty() {
        return false;
    }
    if e.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return false;
    }
    // A comma or semicolon would let a second recipient be smuggled into a
    // header by anything downstream that splits on them.
    if e.contains(',') || e.contains(';') {
        return false;
    }
    let mut parts = e.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !local.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.')
}

/// A display hint: `jo***@example.com`.
///
/// Enough to recognise an account you meant to revoke; not enough to be a
/// contact list if the volume is copied.
pub fn email_hint(email: &str) -> String {
    let e = normalize_email(email);
    let Some((local, domain)) = e.split_once('@') else {
        return "***".to_string();
    };
    let keep: String = local.chars().take(2).collect();
    format!("{keep}***@{domain}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hashed(email: &str) -> String {
        hash(&normalize_email(email))
    }

    #[test]
    fn nothing_that_grants_access_is_serialized() {
        // The volume is backed up and copied around. A session token in it would
        // be a sign-in anyone holding a backup could replay.
        let mut accts = Accounts::default();
        let a = accts.create(&hashed("jo@example.com"), "jo***@example.com", 1);
        let token = accts.open_session(&a.id, 1);

        let json = serde_json::to_string(&accts).unwrap();
        assert!(
            !json.contains(&token),
            "the session token must not be stored"
        );
        assert!(
            !json.contains("jo@example.com"),
            "nor the address in the clear"
        );
    }

    #[test]
    fn an_email_is_stored_hashed_with_a_hint_that_is_not_a_contact_list() {
        // Honest about what this buys: not anonymisation — the address space is
        // brute-forceable — but a copied volume is not a mailing list.
        let hint = email_hint("Jonathan.Smith@Example.COM");
        assert_eq!(hint, "jo***@example.com");
        assert!(!hint.contains("nathan"), "{hint}");
        assert!(!hint.contains("Smith"), "{hint}");
    }

    #[test]
    fn normalization_folds_case_but_does_not_merge_two_people() {
        // Stripping dots or `+tags` is wrong on providers where they are
        // significant, and merging two humans' accounts is worse than the abuse
        // it would prevent.
        assert_eq!(hashed("A@X.com"), hashed("  a@x.com  "));
        assert_ne!(hashed("a.b@x.com"), hashed("ab@x.com"));
        assert_ne!(hashed("a+one@x.com"), hashed("a@x.com"));
    }

    #[test]
    fn a_magic_link_expires() {
        let mut links = Links::default();
        let token = links.issue("h", "j***@x.com", 1_000_000);

        assert!(links.peek(&token, 1_000_000 + LINK_TTL_MS - 1).is_some());
        assert!(links.peek(&token, 1_000_000 + LINK_TTL_MS + 1).is_none());
    }

    #[test]
    fn a_clock_going_backwards_does_not_extend_a_link() {
        // Without `saturating_sub` this underflows into a link valid for the next
        // forty-nine days.
        let mut links = Links::default();
        let token = links.issue("h", "j***@x.com", 1_000_000);
        assert!(links.peek(&token, 999_000).is_some(), "not expired");
        assert!(!links.links[0].expired(999_000));
    }

    #[test]
    fn peeking_at_a_link_consumes_nothing() {
        // Mail scanners prefetch every URL in a message. A GET that spent the
        // token would burn it before the human ever opened their inbox.
        let mut links = Links::default();
        let token = links.issue("h", "j***@x.com", 1000);

        assert!(links.peek(&token, 1000).is_some());
        assert!(links.peek(&token, 1000).is_some(), "still there");
        assert!(links.consume(&token, 1000).is_ok(), "and still spendable");
    }

    #[test]
    fn a_link_is_single_use_and_a_second_click_is_told_apart_from_a_forgery() {
        // "Invalid link" to someone whose sign-in just worked reads as a bug.
        let mut links = Links::default();
        let token = links.issue("h", "j***@x.com", 1000);

        assert!(links.consume(&token, 1000).is_ok());
        assert_eq!(
            links.consume(&token, 1000),
            Err(LinkError::AlreadyUsed),
            "a double click"
        );
        assert_eq!(
            links.consume("fabricated", 1000),
            Err(LinkError::Invalid),
            "and a guess is not"
        );
    }

    #[test]
    fn an_expired_link_looks_the_same_as_a_fabricated_one() {
        // Distinguishing them would confirm that a guessed token once existed.
        let mut links = Links::default();
        let token = links.issue("h", "j***@x.com", 1000);
        assert_eq!(
            links.consume(&token, 1000 + LINK_TTL_MS + 1),
            Err(LinkError::Invalid)
        );
    }

    #[test]
    fn outstanding_counts_only_what_could_still_be_used() {
        // This is the number the mail cap is enforced against, so counting spent
        // or expired links would throttle sending for no reason.
        let mut links = Links::default();
        let a = links.issue("h1", "a***@x.com", 1000);
        links.issue("h2", "b***@x.com", 1000);
        assert_eq!(links.outstanding(1000), 2);

        links.consume(&a, 1000).unwrap();
        assert_eq!(links.outstanding(1000), 1, "spent does not count");
        assert_eq!(
            links.outstanding(1000 + LINK_TTL_MS + 1),
            0,
            "nor does expired"
        );
    }

    #[test]
    fn sweeping_drops_only_links_past_retention() {
        let mut links = Links::default();
        links.issue("h", "j***@x.com", 1000);
        links.sweep(1000 + LINK_RETAIN_MS - 1);
        assert_eq!(links.links.len(), 1, "still explaining a second click");
        links.sweep(1000 + LINK_RETAIN_MS + 1);
        assert!(links.links.is_empty());
    }

    #[test]
    fn revoking_an_account_kills_its_sessions_without_touching_them() {
        // Liveness is derived rather than copied: revocation that depends on
        // remembering to update N session records eventually misses one.
        let mut accts = Accounts::default();
        let a = accts.create(&hashed("jo@x.com"), "jo***@x.com", 1);
        let t1 = accts.open_session(&a.id, 1);
        let t2 = accts.open_session(&a.id, 2);

        assert!(accts.revoke(&a.id));
        assert!(accts.session_for(&t1).is_none());
        assert!(accts.session_for(&t2).is_none(), "every session, at once");
        assert!(
            !accts.sessions[0].revoked,
            "the session record was not walked"
        );
        assert!(!accts.revoke(&a.id), "revoking twice reports no change");
    }

    #[test]
    fn revoking_one_account_leaves_the_others_signed_in() {
        let mut accts = Accounts::default();
        let a = accts.create(&hashed("a@x.com"), "a***@x.com", 1);
        let b = accts.create(&hashed("b@x.com"), "b***@x.com", 1);
        let ta = accts.open_session(&a.id, 1);
        let tb = accts.open_session(&b.id, 1);

        accts.revoke(&a.id);
        assert!(accts.session_for(&ta).is_none());
        assert!(accts.session_for(&tb).is_some());
    }

    #[test]
    fn a_revoked_account_is_kept_and_is_not_silently_recreated() {
        // Signing in again must not hand back what was taken away, or revocation
        // means nothing.
        let mut accts = Accounts::default();
        let a = accts.create(&hashed("jo@x.com"), "jo***@x.com", 1);
        accts.revoke(&a.id);

        assert_eq!(accts.accounts.len(), 1, "kept");
        assert!(accts.live().is_empty(), "but not live");
        assert!(accts.by_email(&hashed("jo@x.com")).is_none());
        assert!(
            accts.any_by_email(&hashed("jo@x.com")).is_some(),
            "and still findable, so signup cannot quietly recreate it"
        );
    }

    #[test]
    fn two_accounts_created_in_the_same_millisecond_get_distinct_ids() {
        // A time-derived id collides here, and revoking one revokes the other.
        let mut accts = Accounts::default();
        let a = accts.create(&hashed("a@x.com"), "a***@x.com", 1000);
        let b = accts.create(&hashed("b@x.com"), "b***@x.com", 1000);
        assert_ne!(a.id, b.id);
    }

    #[test]
    fn an_unknown_session_token_matches_nothing() {
        let mut accts = Accounts::default();
        let a = accts.create(&hashed("jo@x.com"), "jo***@x.com", 1);
        accts.open_session(&a.id, 1);
        assert!(accts.session_for("not-a-real-token").is_none());
        assert!(accts.session_for("").is_none());
    }

    #[test]
    fn email_validation_refuses_what_would_break_something_downstream() {
        for good in ["a@example.com", "jo.smith+tag@sub.example.co.uk", "x@y.io"] {
            assert!(valid_email(good), "{good}");
        }
        for bad in [
            "",
            "   ",
            "no-at-sign",
            "@example.com",
            "a@",
            "a@nodot",
            "a@.leading",
            "a@trailing.",
            "two@at@signs.com",
            // A second recipient smuggled past anything that splits on these.
            "a@x.com,b@y.com",
            "a@x.com;b@y.com",
            // Header injection, refused structurally as well as by the encoder.
            "a@x.com\nBcc: victim@y.com",
            "a@x.com\r\nSubject: spam",
            "a b@x.com",
        ] {
            assert!(!valid_email(bad), "{bad:?} must be refused");
        }
        assert!(!valid_email(&format!("{}@x.com", "a".repeat(300))));
    }

    #[test]
    fn a_store_written_before_a_field_existed_still_loads() {
        // The volume outlives any one image tag.
        let accts: Accounts = serde_json::from_str("{}").unwrap();
        assert!(accts.accounts.is_empty());
        let links: Links = serde_json::from_str("{}").unwrap();
        assert!(links.links.is_empty());
    }

    #[test]
    fn accounts_round_trip_through_json() {
        let mut accts = Accounts::default();
        let a = accts.create(&hashed("jo@x.com"), "jo***@x.com", 1);
        accts.open_session(&a.id, 1);
        let json = serde_json::to_string(&accts).unwrap();
        assert_eq!(serde_json::from_str::<Accounts>(&json).unwrap(), accts);
    }

    #[test]
    fn a_session_sign_in_just_opened_is_freshly_proved() {
        // A credential was typed a moment ago, which is the whole definition of
        // proved.
        let mut accounts = Accounts::default();
        let account = accounts.create(&hash("a@b.test"), "a***@b.test", 1_000);
        let token = accounts.open_session(&account.id, 1_000);
        assert!(accounts.session_fresh(&token, 1_000));
        assert!(accounts.session_fresh(&token, 1_000 + FRESH_AUTH_MS));
        assert!(!accounts.session_fresh(&token, 1_001 + FRESH_AUTH_MS));
    }

    #[test]
    fn proving_it_again_refreshes_the_same_session_rather_than_opening_another() {
        // Otherwise proving yourself again would mean signing out and back in,
        // and the stale session would stay live beside the new one -- two
        // credentials where the point was to refresh one.
        let mut accounts = Accounts::default();
        let account = accounts.create(&hash("a@b.test"), "a***@b.test", 1_000);
        let token = accounts.open_session(&account.id, 1_000);
        let later = 1_000 + FRESH_AUTH_MS * 4;
        assert!(!accounts.session_fresh(&token, later));

        assert!(accounts.refresh_session(&token, later));
        assert!(accounts.session_fresh(&token, later));
        assert_eq!(accounts.sessions.len(), 1, "no second session");
        assert!(
            accounts.session_for(&token).is_some(),
            "and the same cookie still works"
        );
    }

    #[test]
    fn freshness_is_per_session_and_not_per_account() {
        // **The entire meaning of proving it again.** A re-authentication on a
        // laptop must not privilege a phone that has sat signed in for a month.
        let mut accounts = Accounts::default();
        let account = accounts.create(&hash("a@b.test"), "a***@b.test", 1_000);
        let laptop = accounts.open_session(&account.id, 1_000);
        let phone = accounts.open_session(&account.id, 1_000);

        let later = 1_000 + FRESH_AUTH_MS * 4;
        accounts.refresh_session(&laptop, later);
        assert!(accounts.session_fresh(&laptop, later));
        assert!(
            !accounts.session_fresh(&phone, later),
            "the phone was privileged by the laptop"
        );
    }

    #[test]
    fn a_revoked_session_is_never_fresh_and_cannot_be_refreshed() {
        let mut accounts = Accounts::default();
        let account = accounts.create(&hash("a@b.test"), "a***@b.test", 1_000);
        let token = accounts.open_session(&account.id, 1_000);
        assert!(accounts.revoke(&account.id));

        // Revocation is derived from the account, so the session itself is not
        // flipped -- which is exactly why this is worth asserting.
        accounts.sessions[0].revoked = true;
        assert!(!accounts.session_fresh(&token, 1_000));
        assert!(!accounts.refresh_session(&token, 1_000));
    }

    #[test]
    fn a_session_written_before_freshness_existed_reads_as_stale() {
        // Fails closed: an upgrade asks for the hop rather than honouring a
        // month-old session as freshly proved.
        let old = r#"{"accounts":[],"sessions":[{"account_id":"acc-1",
                      "token_hash":"deadbeef","issued_ms":1}],"links":[]}"#;
        let accounts: Accounts = serde_json::from_str(old).unwrap();
        assert_eq!(accounts.sessions[0].authed_ms, 0);
        assert!(!accounts.sessions[0].fresh(1_000_000));
    }

    #[test]
    fn a_clock_stepping_backwards_does_not_make_a_session_fresh() {
        let mut accounts = Accounts::default();
        let account = accounts.create(&hash("a@b.test"), "a***@b.test", 10_000_000);
        let token = accounts.open_session(&account.id, 10_000_000);
        // "Now" is long before the stamp. `saturating_sub` gives 0, which reads
        // as fresh -- correct, since the stamp is in the future and the session
        // was proved more recently than now.
        assert!(accounts.session_fresh(&token, 1));
    }

    #[test]
    fn the_cache_sees_a_revocation_on_the_next_look() {
        // **The property that makes caching a credential store safe at all.**
        // Revocation has to take effect on the request after it; a cache that
        // held its answer would keep a revoked filer signed in until something
        // else happened to change the file.
        let dir = std::env::temp_dir().join(format!(
            "sc-accounts-{}-{}",
            std::process::id(),
            &crate::auth::mint_secret()[..12]
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("accounts.json");

        let mut accounts = Accounts::default();
        let account = accounts.create(&hash("a@b.test"), "a***@b.test", 1_000);
        let id = account.id.clone();
        let token = accounts.open_session(&id, 1_000);
        std::fs::write(&path, serde_json::to_string(&accounts).unwrap()).unwrap();

        let mut cache = AccountsCache::default();
        assert!(cache.current(&path).session_for(&token).is_some());

        accounts.revoke(&id);
        std::fs::write(&path, serde_json::to_string(&accounts).unwrap()).unwrap();
        cache.invalidate();

        assert!(
            cache.current(&path).session_for(&token).is_none(),
            "a revoked account was still signed in"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_accounts_file_is_an_empty_set_and_not_an_error() {
        // The resting state of every fresh install.
        let mut cache = AccountsCache::default();
        let nowhere = std::env::temp_dir().join("sc-accounts-does-not-exist.json");
        assert!(cache.current(&nowhere).accounts.is_empty());
    }

    #[test]
    fn an_unreadable_file_keeps_the_last_good_answer() {
        // A transient error must not sign everybody out at once, which would
        // look exactly like a revocation nobody performed.
        let dir = std::env::temp_dir().join(format!(
            "sc-accounts-bad-{}-{}",
            std::process::id(),
            &crate::auth::mint_secret()[..12]
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("accounts.json");

        let mut accounts = Accounts::default();
        accounts.create(&hash("a@b.test"), "a***@b.test", 1_000);
        std::fs::write(&path, serde_json::to_string(&accounts).unwrap()).unwrap();

        let mut cache = AccountsCache::default();
        assert_eq!(cache.current(&path).accounts.len(), 1);

        std::fs::write(&path, "{ this is not json").unwrap();
        cache.invalidate();
        assert_eq!(
            cache.current(&path).accounts.len(),
            1,
            "a bad parse emptied the store"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_login_account_signs_in_with_its_password() {
        let mut accounts = Accounts::default();
        let account = accounts
            .create_login("JameZ667@example.test", "correct-horse-battery", 1_000)
            .unwrap();

        // Lowercased on the way in, so whatever casing somebody types matches.
        assert_eq!(account.login.as_deref(), Some("jamez667@example.test"));
        assert!(accounts.by_login("JAMEZ667@EXAMPLE.TEST").is_some());

        assert_eq!(
            accounts
                .check_password("jamez667@example.test", "correct-horse-battery", 1_000)
                .as_deref(),
            Ok(account.id.as_str())
        );
    }

    #[test]
    fn one_address_is_one_account_however_it_signs_in() {
        // **The whole point of making the login an email.** A filer who signed
        // in by link last week and is given a password today is the same person,
        // and used to become two unrelated rows: one keyed on `hash(address)`,
        // one on a synthesised `hash("login:name")`. Nothing could reconcile
        // them, so their filings sat under an identity their permissions did not.
        let mut accounts = Accounts::default();
        let email = "jo@x.com";
        let hash_of = crate::auth::hash(&normalize_email(email));

        // They arrive as a filer.
        let filed_as = accounts.create(&hash_of, &email_hint(email), 1_000).id;
        assert_eq!(accounts.accounts.len(), 1);

        // They are given a password.
        let with_password = accounts
            .create_login(email, "correct-horse-battery", 2_000)
            .expect("the address is theirs");

        assert_eq!(accounts.accounts.len(), 1, "adopted, not duplicated");
        assert_eq!(with_password.id, filed_as, "and it is the same account");

        // Both ways in now land on it.
        assert_eq!(
            accounts.check_password(email, "correct-horse-battery", 3_000),
            Ok(filed_as.clone()),
        );
        assert_eq!(
            accounts.by_email(&hash_of).map(|a| a.id.clone()),
            Some(filed_as),
            "and a magic link resolves the very same row"
        );
    }

    #[test]
    fn a_capitalised_address_is_the_same_account() {
        // Somebody registers one way and types it another. A second row here
        // would be an account they could not reach and could not see.
        let mut accounts = Accounts::default();
        accounts
            .create_login("Jo@X.com", "correct-horse-battery", 1_000)
            .unwrap();
        assert!(accounts
            .create_login("jo@x.com", "another-password-here", 2_000)
            .is_err());
        assert_eq!(accounts.accounts.len(), 1);
        assert!(accounts
            .check_password("JO@X.COM", "correct-horse-battery", 3_000)
            .is_ok());
    }

    #[test]
    fn a_login_that_is_not_an_address_is_refused() {
        // It keys the account, so it has to be the thing a magic link would
        // hash to. A bare name would key on nothing anybody could sign in to.
        let mut accounts = Accounts::default();
        for bad in ["jamez667", "", "no-at-sign.com", "@x.com"] {
            assert!(
                accounts
                    .create_login(bad, "correct-horse-battery", 1)
                    .is_err(),
                "{bad:?} was accepted"
            );
        }
    }

    #[test]
    fn two_accounts_cannot_share_a_login() {
        // **Nothing enforced this while logins came from GitHub**, because a
        // third party guaranteed uniqueness. Two accounts sharing one would make
        // `Admin::is` match whichever the iteration reached first.
        let mut accounts = Accounts::default();
        accounts
            .create_login("jamez667@example.test", "correct-horse-battery", 1)
            .unwrap();
        let err = accounts
            .create_login("JameZ667@example.test", "another-good-password", 2)
            .unwrap_err();
        assert!(err.contains("taken"), "{err}");
        assert_eq!(accounts.accounts.len(), 1);
    }

    #[test]
    fn a_password_is_never_stored_in_the_clear() {
        let mut accounts = Accounts::default();
        accounts
            .create_login("jamez667@example.test", "correct-horse-battery", 1)
            .unwrap();
        let json = serde_json::to_string(&accounts).unwrap();
        assert!(!json.contains("correct-horse-battery"), "{json}");
        assert!(
            json.contains("$argon2id$"),
            "and it is the slow hash: {json}"
        );
    }

    #[test]
    fn a_wrong_password_backs_off_and_a_right_one_clears_it() {
        let mut accounts = Accounts::default();
        accounts
            .create_login("jamez667@example.test", "correct-horse-battery", 0)
            .unwrap();

        // The first two are free: a typo should not be punished, and the point
        // is to make *sustained* guessing expensive.
        for _ in 0..2 {
            assert_eq!(
                accounts.check_password("jamez667@example.test", "wrong", 0),
                Err(0)
            );
        }
        // Then it grows.
        let third = accounts
            .check_password("jamez667@example.test", "wrong", 0)
            .unwrap_err();
        assert!(third > 0, "the third failure did not delay");
        let fourth = accounts
            .check_password("jamez667@example.test", "wrong", third)
            .unwrap_err();
        assert!(fourth > third + 1_000, "the delay did not grow");

        // And a correct one wipes it, so a forgotten password is a short wait
        // rather than something to recover from.
        assert!(accounts
            .check_password("jamez667@example.test", "correct-horse-battery", fourth)
            .is_ok());
        assert_eq!(
            accounts
                .by_login("jamez667@example.test")
                .unwrap()
                .failed_attempts,
            0
        );
        assert_eq!(
            accounts
                .by_login("jamez667@example.test")
                .unwrap()
                .retry_at(),
            0
        );
    }

    #[test]
    fn a_backed_off_account_is_refused_without_checking_the_password() {
        // **Checked before the hash is computed**, so a locked-out account is
        // not also a way to spend this server's CPU — which argon2 makes a real
        // cost rather than a theoretical one.
        let mut accounts = Accounts::default();
        accounts
            .create_login("jamez667@example.test", "correct-horse-battery", 0)
            .unwrap();
        for _ in 0..4 {
            let _ = accounts.check_password("jamez667@example.test", "wrong", 0);
        }
        let until = accounts
            .by_login("jamez667@example.test")
            .unwrap()
            .retry_at();
        assert!(until > 0);

        // Even the RIGHT password is refused while the delay stands.
        assert_eq!(
            accounts.check_password("jamez667@example.test", "correct-horse-battery", until - 1),
            Err(until)
        );
        // And works once it passes.
        assert!(accounts
            .check_password("jamez667@example.test", "correct-horse-battery", until)
            .is_ok());
    }

    #[test]
    fn the_backoff_is_bounded() {
        // A ceiling, not a lockout: with no second way in, locking the
        // administrator out means editing a file on the volume, and anybody who
        // knows the username could trigger that deliberately.
        assert_eq!(backoff_ms(0), 0);
        assert_eq!(backoff_ms(2), 0);
        assert!(backoff_ms(3) > 0);
        assert_eq!(backoff_ms(100), MAX_BACKOFF_MS);
        assert!(backoff_ms(u32::MAX) <= MAX_BACKOFF_MS, "no overflow");
    }

    #[test]
    fn a_missing_account_and_a_wrong_password_are_one_answer() {
        // Distinguishing them tells a guesser which half they got right.
        let mut accounts = Accounts::default();
        accounts
            .create_login("jamez667@example.test", "correct-horse-battery", 0)
            .unwrap();
        assert_eq!(accounts.check_password("nobody", "whatever", 0), Err(0));
        assert_eq!(
            accounts.check_password("jamez667@example.test", "wrong", 0),
            Err(0)
        );
    }

    #[test]
    fn a_revoked_account_cannot_sign_in_and_frees_its_login() {
        let mut accounts = Accounts::default();
        let a = accounts
            .create_login("jamez667@example.test", "correct-horse-battery", 0)
            .unwrap();
        assert!(accounts.revoke(&a.id));

        assert!(accounts.by_login("jamez667@example.test").is_none());
        assert_eq!(
            accounts.check_password("jamez667@example.test", "correct-horse-battery", 0),
            Err(0)
        );
        // And the name can be used again, since the old record grants nothing.
        assert!(accounts
            .create_login("jamez667@example.test", "a-different-password", 1)
            .is_ok());
    }

    #[test]
    fn a_filer_has_no_login_and_that_is_not_an_error() {
        // Magic links stay for filers: strangers should not be made to keep a
        // credential.
        let mut accounts = Accounts::default();
        let a = accounts.create(&hash("jo@x.com"), "j***@x.com", 1);
        assert!(a.login.is_none());
        assert!(a.password_hash.is_none());
        assert!(accounts.by_login("jo@x.com").is_none());
    }

    #[test]
    fn an_account_written_before_passwords_existed_still_loads() {
        // The data volume outlives any one image tag.
        let old = r#"{"accounts":[{"id":"acct-1","email_hash":"x",
                      "email_hint":"j***@x.com","created_ms":1,"revoked":false}],
                      "sessions":[],"links":[]}"#;
        let accounts: Accounts = serde_json::from_str(old).unwrap();
        let a = &accounts.accounts[0];
        assert!(a.login.is_none());
        assert!(a.password_hash.is_none());
        assert_eq!(a.failed_attempts, 0);
        assert_eq!(a.retry_at(), 0);
    }
}
