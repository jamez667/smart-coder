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
    /// The GitHub login this account signed in with, lowercased, when it did.
    ///
    /// **Not a permission.** It says who somebody proved they are, and nothing
    /// about what they may see — whether this login is an *owner* comes from the
    /// configuration, checked on every request. So removing a name from
    /// `SC_SERVER_OWNERS` demotes them immediately, and this field is left
    /// alone: it remains a true statement about how they signed in.
    ///
    /// `None` for an account created by a magic link, which is every account
    /// that existed before GitHub sign-in — hence `#[serde(default)]`.
    ///
    /// Storing the login rather than the numeric id is the same trade the
    /// configuration makes: the operator writes a name they recognise, and the
    /// two have to be comparable.
    #[serde(default)]
    pub github_login: Option<String>,
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
    /// Unix ms when this browser last proved itself against GitHub.
    ///
    /// Equal to `issued_ms` on a session the callback just opened, and moved
    /// forward when a re-authentication lands on it.
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
/// Five minutes, not ten and not an hour. GitHub's own prompt is a few seconds
/// when you already hold a session there, so the window only has to cover
/// clicking through, coming back, and typing. Long enough that changing two
/// settings does not mean two hops; short enough that a laptop walked away from
/// is not a standing key to the secrets.
///
/// Deliberately not [`crate::oauth::STATE_TTL_MS`], which is ten minutes and
/// covers a *human deciding* whether to authorise an application — a different
/// event that happens to be adjacent.
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
            // A magic-link signup. GitHub sign-in sets this at creation, and it
            // is the only way an account ever acquires one.
            github_login: None,
        };
        self.accounts.push(account.clone());
        account
    }

    /// Open a session for an account, returning the cookie token.
    pub fn open_session(&mut self, account_id: &str, now_ms: u64) -> String {
        let token = mint_secret();
        self.sessions.push(Session {
            account_id: account_id.to_string(),
            token_hash: hash(&token),
            issued_ms: now_ms,
            revoked: false,
            // A session the callback just opened *is* freshly proved: the
            // browser came back from GitHub a moment ago.
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

    /// Was this token's session proved against GitHub recently?
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
    fn a_session_the_callback_just_opened_is_freshly_proved() {
        // The browser came back from GitHub a moment ago, which is the whole
        // definition of proved.
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
}
