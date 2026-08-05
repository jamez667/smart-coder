// The words this interface draws itself out of.
//
// **The server owns the catalogue and this holds a copy.** There is no English
// text in a component any more; every visible string is a field on the object
// fetched from `GET /api/v1/ui/strings`, in the language that request
// negotiated. The alternative — English in the components with a translation
// layer over it — means the English is a fallback that renders when a lookup
// misses, and a missed lookup is exactly what nobody notices until a reader
// says so. Here there is nothing to fall back *to*, so a missing string is a
// visible gap during development rather than an English word on a French page.

import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useRef,
  useState,
  type ReactNode,
} from "react";

/// Every field the catalogue carries.
///
/// **Hand-written against `crates/sc-server/src/i18n.rs`**, and it has to be:
/// TypeScript cannot read a Rust struct. What keeps the two in step is that a
/// field named here and absent there is a compile error at the first use — the
/// object is typed, not indexed by string — and a field added there and not
/// here is simply unused, which is the harmless direction for the drift to run.
///
/// The Rust side has the stronger guarantee and keeps it: a language missing a
/// field does not compile, and a field left at its English text fails a test.
/// This is the projection of that onto a client that cannot participate in it.
export interface Strings {
  // the shell
  brand: string;
  theme_light: string;
  theme_dark: string;
  theme_to_light: string;
  theme_to_dark: string;
  language_label: string;
  language_apply: string;
  footer_tagline: string;
  footer_tagline_app: string;
  nav_signin: string;
  nav_account: string;
  nav_mine: string;
  nav_review: string;
  nav_signout: string;
  nav_admin_heading: string;
  nav_admin_review: string;
  nav_admin_settings: string;
  nav_admin_repos: string;
  nav_admin_owners: string;
  nav_admin_daemons: string;
  nav_admin_accounts: string;
  dialog_close: string;

  // the landing page
  landing_headline: string;
  landing_sub: string;
  landing_point_1_title: string;
  landing_point_1_body: string;
  landing_point_2_title: string;
  landing_point_2_body: string;
  landing_point_3_title: string;
  landing_point_3_body: string;

  // signing in
  signin_title: string;
  signin_intro: string;
  signin_email_label: string;
  signin_email_placeholder: string;
  signin_submit: string;
  signin_sent: string;
  signin_no_password_note: string;
  signin_password_heading: string;
  signin_password_label: string;
  signin_password_submit: string;

  // filing
  filing_heading: string;
  filing_text_label: string;
  filing_text_placeholder: string;
  filing_kind_label: string;
  kind_feature: string;
  kind_bug: string;
  filing_repo_label: string;
  filing_submit: string;
  filing_done: string;
  filing_none_title: string;
  filing_none_body: string;
  file_mine_heading: string;

  // reviewing
  review_heading: string;
  review_empty_title: string;
  review_empty_body: string;
  review_no_daemon: string;
  review_unserved: string;
  review_skip_to_decision: string;
  review_asked_heading: string;
  review_spec_heading: string;
  review_note_heading: string;
  review_landed_before: string;
  review_decide_heading: string;
  review_send_back_label: string;
  review_send_back_placeholder: string;
  review_send_back_submit: string;
  review_accept: string;
  review_discard: string;
  review_leaving_decides_nothing: string;
  review_quarantine_leaving: string;
  owner_release: string;

  // the raw wire states and kinds, given faces
  review_state_screening: string;
  review_state_quarantined: string;
  review_state_queued: string;
  review_state_claimed: string;
  review_state_awaiting_review: string;
  review_state_accepted: string;
  review_state_discarded: string;
  review_state_failed: string;

  // the interface's own 404
  app_not_found_title: string;
  app_not_found_body: string;
  app_not_found_link: string;

  // administering
  admin_saved: string;
  admin_save: string;
  admin_add: string;
  admin_revoke: string;
  admin_revoked_tag: string;
  settings_heading: string;
  settings_public_heading: string;
  settings_public_note: string;
  settings_public_on: string;
  settings_public_off: string;
  settings_filers_heading: string;
  settings_show_spec: string;
  settings_stack_heading: string;
  settings_stack_note: string;
  settings_ceilings_heading: string;
  settings_ceilings_note: string;
  settings_max_filings: string;
  settings_max_drafts: string;
  settings_max_accounts: string;
  settings_max_links: string;
  owners_heading: string;
  owners_note: string;
  owners_add_heading: string;
  owners_email_label: string;
  owners_repos_label: string;
  repos_heading: string;
  repos_unserved_before: string;
  repos_unserved_after: string;
  repos_enable_anyway: string;
  repos_no_machine: string;
  repos_off_tag: string;
  repos_turn_off: string;
  repos_add_heading: string;
  repos_name_label: string;
  repos_enable: string;
  daemons_heading: string;
  daemons_minted_after: string;
  daemons_add_heading: string;
  daemons_label_label: string;
  daemons_mint: string;
  accounts_heading: string;
  accounts_note: string;
  accounts_password_tag: string;

  // the setup wizard
  setup_code_heading: string;
  setup_code_intro: string;
  setup_code_label: string;
  setup_base_url_before: string;
  setup_base_url_after: string;
  setup_continue: string;
  setup_admin_heading: string;
  setup_admin_intro: string;
  setup_admin_intro_strong: string;
  setup_email_label: string;
  setup_password_label: string;
  setup_min_password_before: string;
  setup_min_password_chars: string;
  setup_min_password_chars_one: string;
  setup_min_password_after: string;
  setup_min_password_tail: string;
  setup_claim: string;
}

/// What the endpoint sends.
export interface Catalogue {
  /// The language actually negotiated, which is not necessarily what the
  /// browser asked for. Stamped on `<html lang>`.
  locale: string;
  strings: Strings;
}

/// A catalogue as it sits in `localStorage`, with the validator that lets a
/// revalidation be answered 304.
interface Cached extends Catalogue {
  /// The `ETag` the server sent with these exact bytes.
  ///
  /// **Stored beside the strings rather than derived from them.** Re-hashing the
  /// body on the client to reconstruct a tag would mean matching the server's
  /// hash function forever; storing what it sent means the client never has an
  /// opinion about how the tag is computed.
  etag: string;
}

/// The languages the switcher offers, and their names **in themselves**.
///
/// "Français", never "French": somebody who cannot read the current page is
/// exactly the person using this control, so listing the options in a language
/// they may not read defeats it. Mirrors `Locale::endonym` on the server; the
/// server remains the authority on which catalogues exist, and a code listed
/// here that it does not have simply negotiates back to the default.
export const LANGUAGES: { code: string; endonym: string }[] = [
  { code: "en", endonym: "English" },
  { code: "fr", endonym: "Français" },
];

/// Where a catalogue is kept between visits.
///
/// **Keyed on the locale**, so switching language does not throw away the
/// catalogue for the language switched away from — a reader toggling between two
/// gets both from cache after the first visit to each.
const KEY = (locale: string) => `sc.strings.${locale}`;

/// Which locale the cache should be looked up under on a cold load.
///
/// The cache is keyed on a locale, and the first render has not asked the server
/// yet — so something has to say which key to try.
///
/// **The cookie is not enough, and assuming it was is what broke this.** The
/// `sc_lang` cookie is written only when a reader *chooses* a language; a French
/// browser that has never touched the switcher is negotiated to French from
/// `Accept-Language` and has no cookie at all. Reading the cookie alone meant
/// that reader's cache was written under `sc.strings.fr` and then never looked
/// for — so every load re-fetched, `If-None-Match` was never sent, and the whole
/// revalidation path was dead while looking perfectly healthy.
///
/// So the last negotiated locale is remembered here too. The cookie still wins
/// where it exists, because it is the deliberate choice and the server honours
/// it over the header; this is the fallback for the much more common reader who
/// has never expressed one.
const LAST = "sc.strings.last";

function cachedLocale(): string | null {
  const hit = document.cookie
    .split(";")
    .map((c) => c.trim())
    .find((c) => c.startsWith("sc_lang="));
  if (hit) return decodeURIComponent(hit.slice("sc_lang=".length));
  try {
    return window.localStorage.getItem(LAST);
  } catch {
    return null;
  }
}

/// The catalogue this browser already has, if any.
///
/// Returns `null` rather than throwing on anything unexpected. `localStorage`
/// can be disabled, full, or holding JSON written by an older build of this
/// interface — and every one of those is "we have no cache", which is a state
/// this already handles. Throwing would turn a storage quirk into a blank page.
function readCache(locale: string): Cached | null {
  try {
    const raw = window.localStorage.getItem(KEY(locale));
    if (!raw) return null;
    const parsed = JSON.parse(raw) as Partial<Cached>;
    // A shape check rather than a version number. What could be in there is an
    // object written by a build whose `Strings` had different fields, and the
    // ETag catches that the moment the server answers — see `useCatalogue`.
    if (!parsed.etag || !parsed.locale || !parsed.strings) return null;
    return parsed as Cached;
  } catch {
    return null;
  }
}

function writeCache(value: Cached) {
  try {
    window.localStorage.setItem(KEY(value.locale), JSON.stringify(value));
    // **And which locale to look under next time.** Written with the catalogue
    // rather than separately, so the pointer cannot name a key that was never
    // stored. See `cachedLocale` for why the cookie alone is not enough.
    window.localStorage.setItem(LAST, value.locale);
  } catch {
    // Quota, private mode, or storage switched off. The interface works without
    // a cache — it just fetches every load — so this is not worth surfacing.
  }
}

const Ctx = createContext<Catalogue | null>(null);

/// The provider's setter, in its own context.
///
/// **Split from the catalogue rather than passed beside it in one object.** A
/// single context carrying `{catalogue, set}` changes identity whenever either
/// half does, so every component that merely *reads* a string — which is every
/// component in this interface — would re-render when the setter was
/// reconstructed. Two contexts means the reading path is one lookup that only
/// changes when the words do.
const SetCtx = createContext<((c: Cached) => void) | null>(null);

/// The strings, for a component that draws something.
///
/// Throws rather than returning `null` when there is no provider above it: that
/// is a wiring mistake in this file's own tree, not a runtime condition, and a
/// component silently rendering nothing is much harder to find than a stack
/// trace naming the hook.
export function useStrings(): Strings {
  const c = useContext(Ctx);
  if (!c) throw new Error("useStrings outside a StringsProvider");
  return c.strings;
}

/// The negotiated language code, for the two places that need the code itself
/// rather than a string out of the catalogue: `<html lang>` and the switcher.
export function useLocale(): string {
  const c = useContext(Ctx);
  if (!c) throw new Error("useLocale outside a StringsProvider");
  return c.locale;
}

/// A reviewer's word for a raw state.
///
/// **`ReviewRequest.state` arrives as a wire value** — `queued`, `quarantined`,
/// `awaiting-review` — unlike `FiledRequest.state`, which the server has already
/// translated into the coarse label a filer is allowed to see. Those wire values
/// were being rendered straight into the page, so a reviewer read a variable
/// name where a word belonged, in English, whatever language the rest of the
/// page was in.
///
/// An unknown state falls back to the wire value rather than to a blank or a
/// placeholder. A state added on the server and not here is a word out of place
/// in one badge; a blank is a badge that says nothing at all, and a reviewer
/// cannot tell that apart from a bug in the request itself.
export function stateLabel(s: Strings, state: string): string {
  switch (state) {
    case "screening":
      return s.review_state_screening;
    case "quarantined":
      return s.review_state_quarantined;
    case "queued":
      return s.review_state_queued;
    case "claimed":
      return s.review_state_claimed;
    case "awaitingreview":
    case "awaiting-review":
      return s.review_state_awaiting_review;
    case "accepted":
      return s.review_state_accepted;
    case "discarded":
      return s.review_state_discarded;
    case "failed":
      return s.review_state_failed;
    default:
      return state;
  }
}

/// A reader's word for an intake kind.
///
/// The same problem as `stateLabel`, one field over: `kind` is the slug the form
/// submitted and the server matches on. **The slug stays on the wire and only
/// the label translates** — translating the value would have a filer and a
/// reviewer naming the same kind differently, which is the reason the server's
/// catalogue keeps `kind_feature` and `kind_bug` as labels rather than values.
export function kindLabel(s: Strings, kind: string): string {
  switch (kind) {
    case "feature":
      return s.kind_feature;
    case "bug":
      return s.kind_bug;
    default:
      return kind;
  }
}

/// Fetch the catalogue, rendering from cache while it is in flight.
///
/// ## Cache-first, then revalidate — and why that is safe here
///
/// The catalogue is `&'static str` compiled into the server binary. It **cannot
/// change while that process runs**: altering one string means rebuilding the
/// image and redeploying, which replaces the process. So a cached copy is either
/// exactly right or belongs to a previous deploy, and there is no third state
/// where it is subtly stale mid-session.
///
/// That is what makes rendering from cache before any request lands correct
/// rather than merely fast. The alternative is a blank page on every load while
/// a request that will almost always return "unchanged" completes.
///
/// ## What stops a stale cache outliving a deploy
///
/// The revalidation, which runs on **every** load rather than on a timer. The
/// server sends an `ETag` over the exact bytes; the client stores it beside the
/// strings and sends it back as `If-None-Match`. Three outcomes:
///
/// - **304** — the deploy did not change this catalogue. Nothing is redrawn and
///   nothing is written.
/// - **200 with a different body** — a deploy changed it. The new catalogue
///   replaces the cache and the interface redraws, one render behind the load.
/// - **the request fails** — the reader keeps what they had, which is the whole
///   point of having it.
///
/// A time-based expiry was considered and rejected: any TTL is either shorter
/// than needed (re-fetching a resource that changes on deploys, several times a
/// day, for nothing) or longer (a mistranslation fixed in production still on
/// the reader's screen until it lapses). The ETag ties invalidation to the only
/// event that can actually invalidate this, which is the deploy itself.
///
/// **The locale is part of the cache key and part of the tag's coverage.** The
/// tag is over a body that carries the locale code, so a reader holding the
/// English catalogue and asking as French cannot be answered 304 — the tags do
/// not match, and they were never going to.
export function StringsProvider({ children }: { children: ReactNode }) {
  // The first paint comes from here, so this reads storage synchronously rather
  // than in an effect: a `useEffect` that sets it would render `null` first, and
  // the flash of an empty page is exactly what the cache exists to remove.
  const [catalogue, setCatalogue] = useState<Cached | null>(() => {
    // The locale a previous visit settled on — a chosen one from the cookie, or
    // the last negotiated one. With neither, this browser has never been here
    // and there is nothing to render from; guessing at `navigator.language`
    // would be a *second* negotiation able to disagree with the server's, and a
    // first paint in one language replaced by a second is worse than waiting.
    const locale = cachedLocale();
    return locale ? readCache(locale) : null;
  });

  // The tag as it was when this mounted, held where the revalidation below can
  // read it **without depending on the state it sets**.
  //
  // Written this way rather than by listing `catalogue` and silencing the
  // dependency warning: an effect that both reads and sets the same state is a
  // loop waiting for a condition to be dropped, and a suppression comment is a
  // claim a future reader has to re-derive. A ref makes the "once, with the tag
  // we started with" intent structural.
  const mountedWith = useRef(catalogue?.etag ?? "");

  useEffect(() => {
    // Revalidate on every load. Cheap when nothing changed — a 304 with no body
    // — and it is the only thing that retires a catalogue from a previous
    // deploy. `If-None-Match` is sent only when there is something to validate.
    const headers: Record<string, string> = { Accept: "application/json" };
    if (mountedWith.current) headers["If-None-Match"] = mountedWith.current;

    let live = true;
    fetch("/api/v1/ui/strings", {
      credentials: "same-origin",
      headers,
    })
      .then(async (res) => {
        if (!live) return;
        // Still ours. Nothing to write and nothing to redraw.
        if (res.status === 304) return;
        if (!res.ok) return;
        const etag = res.headers.get("ETag");
        const body = (await res.json()) as Catalogue;
        // No ETag means something between here and the server stripped it — a
        // proxy that rewrites headers, most likely. The strings are still good,
        // so they are used; they are just not cached, because a cache with no
        // validator is the stale-forever case this design exists to avoid.
        if (!etag) {
          setCatalogue({ ...body, etag: "" });
          return;
        }
        const next: Cached = { ...body, etag };
        writeCache(next);
        setCatalogue(next);
      })
      .catch(() => {
        // Offline, or the server is down. Whatever is cached stays on screen —
        // an interface in the reader's language beats an error about strings.
      });
    return () => {
      live = false;
    };
    // **Once per mount.** The only input is the ref above, which is why this
    // list is honestly empty rather than emptied by suppression. A language
    // switch does not come through here at all — `useSetLanguage` brings its own
    // catalogue back with the response that sets the cookie.
  }, []);

  // `<html lang>` is `en` in the served document because the document is one
  // static file for every language. The negotiated locale is only known here, so
  // this is where it gets corrected — and it matters beyond tidiness: a screen
  // reader picks its pronunciation from this attribute, and French read aloud in
  // an English voice is not merely wrong but unintelligible.
  useEffect(() => {
    if (catalogue) document.documentElement.lang = catalogue.locale;
  }, [catalogue]);

  if (!catalogue) {
    // Deliberately blank rather than a spinner, matching what `App` does while
    // `/me` is in flight: on the connection this surface is designed for, a
    // spinner that resolves in 40ms is a flash of noise. A first-time visitor
    // sees this; anybody who has been here before renders from cache instead.
    return <div className="bar-inner" />;
  }

  return (
    <SetCtx.Provider value={setCatalogue}>
      <Ctx.Provider value={catalogue}>{children}</Ctx.Provider>
    </SetCtx.Provider>
  );
}

/// Choose a language.
///
/// **One round trip, not two.** The endpoint sets the cookie the server
/// negotiates on *and* answers the catalogue it just switched to, so the
/// interface redraws immediately rather than posting, re-fetching, and drawing
/// the page twice in the language the reader has just rejected.
///
/// Exported as a hook rather than a bare function because it has to reach the
/// provider's state — and because the cookie and the rendered strings must
/// change together or the next reload disagrees with this one.
/// Change language by *reading* the catalogue rather than choosing one.
///
/// The fallback for when `POST /language` is refused — see the caller. `?lang=`
/// on the read is honoured by the server precisely so this path exists: fetching
/// strings in a named language changes nothing and reveals nothing, since the
/// catalogue is compiled in and identical for every caller.
///
/// The result is deliberately **not** written to the cache. Nothing was chosen
/// on the server, so the next load negotiates afresh; caching it here would make
/// a language stick with no cookie saying why, which is harder to explain than a
/// switch that does not survive a reload.
function switchByReading(code: string, set: (c: Cached) => void): void {
  fetch(`/api/v1/ui/strings?lang=${encodeURIComponent(code)}`, {
    credentials: "same-origin",
    headers: { Accept: "application/json" },
  })
    .then(async (res) => {
      if (!res.ok) return;
      const body = (await res.json()) as Catalogue;
      set({ ...body, etag: "" });
    })
    .catch(() => {
      // Offline, or the server is down. The page stays in the language it was
      // in, which is the honest outcome — nothing changed anywhere.
    });
}

export function useSetLanguage(): (code: string) => void {
  const current = useContext(Ctx);
  const set = useContext(SetCtx);
  return useCallback(
    (code: string) => {
      if (!set || code === current?.locale) return;
      fetch("/api/v1/ui/language", {
        method: "POST",
        credentials: "same-origin",
        // The content type the API demands on every mutating call — a `<form>`
        // cannot send it, which is half of what keeps a cross-origin page off
        // these endpoints.
        headers: {
          "Content-Type": "application/json",
          Accept: "application/json",
        },
        body: JSON.stringify({ lang: code }),
      })
        .then(async (res) => {
          // **A refusal is not the end of it.** Setting the cookie is a mutating
          // call, and on a server with no configured address `same_origin` has
          // nothing to compare an `Origin` against and refuses every one of them
          // — so on a fresh deployment this POST always 403s. Falling back to
          // reading the catalogue in the named language means the switcher works
          // there too; what is lost is only the cookie, so the choice does not
          // survive a reload until the server has an address.
          if (!res.ok) return switchByReading(code, set);
          const etag = res.headers.get("ETag") ?? "";
          const body = (await res.json()) as Catalogue;
          const next: Cached = { ...body, etag };
          if (etag) writeCache(next);
          set(next);
        })
        .catch(() => switchByReading(code, set));
    },
    [current, set],
  );
}

