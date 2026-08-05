// The one place this interface talks to the server.
//
// **Every call goes through here**, so the two things every mutating request
// needs — `Content-Type: application/json` and same-origin credentials — are
// written once rather than at each call site. A `fetch` written by hand
// somewhere else would work right up until it was the one that mattered.

/// Who the server says we are, and what it will let us do.
///
/// **The capabilities are the server's answer, not ours.** Hiding a button the
/// server would refuse is a courtesy to the reader; deciding on the client what
/// somebody may do is a hole. Every route still checks.
export interface Me {
  role: "anonymous" | "filer" | "owner" | "administrator";
  login?: string;
  can: {
    file: boolean;
    review: boolean;
    accept: boolean;
    administer: boolean;
  };
  repos?: string[];
}

/// A request as its own filer may see it.
///
/// Note what is **absent**: no repository, no artifact directory, no daemon
/// note. The server has a separate type for this and never sends those fields,
/// so there is nothing here to accidentally render.
export interface FiledRequest {
  id: string;
  summary: string;
  /// A coarse label, already translated by the server — "received", "writing".
  /// Never the raw state: a filer learning theirs was *quarantined* learns this
  /// server screens, which is what a spammer tunes against.
  state: string;
  kind: string;
  filed_ms: number;
  text?: string;
  spec?: string;
}

/// A request as a reviewer may see it.
export interface ReviewRequest {
  id: string;
  summary: string;
  state: string;
  kind: string;
  repo: string;
  filed_ms: number;
  drafted_ms?: number;
  text?: string;
  spec?: string;
  note?: string;
  /// Administrator only — a path on their own machine.
  artifact_dir?: string;
  /// Whether any machine currently offers to draft this repository:
  /// `served`, `no-daemon-seen` or `unserved`. **The answer to "why has nothing
  /// happened to this"**, and the last two send an operator to different places.
  coverage?: "served" | "no-daemon-seen" | "unserved";
  /// **Thread this back when accepting.** It binds the accept to the exact bytes
  /// that were read: if a redraft lands in between, the digest stops matching
  /// and the server refuses rather than approving text nobody saw.
  spec_digest?: string;
}

export class ApiError extends Error {
  constructor(
    readonly status: number,
    message: string,
  ) {
    super(message);
  }
}

async function parse(res: Response): Promise<unknown> {
  const text = await res.text();
  if (!res.ok) {
    // The server answers JSON even when it refuses — that is why the API
    // dispatch sits above the private device gate, which answers HTML.
    let message = text;
    try {
      const body = JSON.parse(text) as { error?: string };
      if (body.error) message = body.error;
    } catch {
      // A non-JSON error body means something upstream of the server answered.
    }
    throw new ApiError(res.status, message);
  }
  return text ? JSON.parse(text) : null;
}

async function get<T>(path: string): Promise<T> {
  const res = await fetch(`/api/v1/ui/${path}`, {
    // Same-origin, so the session cookie travels and `SameSite=Strict` still
    // means what it meant for a form.
    credentials: "same-origin",
    headers: { Accept: "application/json" },
  });
  return (await parse(res)) as T;
}

async function post<T>(path: string, body: unknown): Promise<T> {
  const res = await fetch(`/api/v1/ui/${path}`, {
    method: "POST",
    credentials: "same-origin",
    // **Not decoration.** A `<form>` cannot send this content type, so demanding
    // it means a cross-origin page cannot reach a mutating endpoint without a
    // preflight — which the server's `Origin` check then fails.
    headers: { "Content-Type": "application/json", Accept: "application/json" },
    body: JSON.stringify(body ?? {}),
  });
  return (await parse(res)) as T;
}

export const api = {
  me: () => get<Me>("me"),

  /// Ask for a magic link.
  ///
  /// **Resolves the same way whatever happened**, because the server answers the
  /// same way: an address with an account, one without, and one over the cap are
  /// indistinguishable on purpose. A client that surfaced the difference would
  /// undo that, so there is nothing here to surface.
  requestLink: (email: string) => post<{ sent: boolean }>("signin", { email }),
  /// Sign in with a password. Rejects with one message for every failure — the
  /// server will not say which, and neither should the dialog.
  signInWithPassword: (login: string, password: string) =>
    post<{ signed_in: boolean }>("signin/password", { login, password }),
  /// Sign out. **Revokes the session on the server**, rather than only dropping
  /// the cookie here — a token this browser forgot but the server still honours
  /// is not signed out.
  signOut: () => post<{ signed_out: boolean }>("signout", {}),

  /// File a request. Returns it as its own filer may see it.
  ///
  /// `repo` may be omitted when the surface serves exactly one — the server
  /// takes the only one. With several it is required, because **the server
  /// never falls back to a default**: filing against a repository nobody chose
  /// would land the work somewhere else with nothing saying so.
  file: (text: string, kind: string, repo?: string) =>
    post<FiledRequest>("file", { text, kind, repo }),
  requests: <T = FiledRequest[] | ReviewRequest[]>() => get<T>("requests"),
  request: <T = FiledRequest | ReviewRequest>(id: string) =>
    get<T>(`requests/${encodeURIComponent(id)}`),

  sendBack: (id: string, note: string) =>
    post<ReviewRequest>(`requests/${encodeURIComponent(id)}/send-back`, { note }),
  discard: (id: string) =>
    post<ReviewRequest>(`requests/${encodeURIComponent(id)}/discard`, {}),
  release: (id: string) =>
    post<ReviewRequest>(`requests/${encodeURIComponent(id)}/release`, {}),
  /// Accepting needs the digest of the spec that was read. See
  /// [`ReviewRequest.spec_digest`].
  accept: (id: string, digest: string) =>
    post<ReviewRequest>(`requests/${encodeURIComponent(id)}/accept`, { digest }),
};

/// The settings, as the interface may see them.
///
/// **Presence and a date, never a value.** There is no read path for a stored
/// secret anywhere in this server, and the API does not add one — so the key
/// fields here are booleans, and the forms that write them are always blank.
export interface SettingsView {
  // The address, the site name, the mail settings and the screener are all
  // environment variables — there is nothing here to read or write. What is
  // left is operational: a switch, a flag and four ceilings.
  public: boolean;
  show_spec: boolean | null;
  max_daily_filings: number | null;
  max_daily_drafts: number | null;
  max_accounts: number | null;
  max_outstanding_links: number | null;
}

export interface OwnerRecord {
  login: string;
  repos: string[];
  added_ms: number;
  revoked: boolean;
}

export interface RepoRecord {
  name: string;
  served_by: string | null;
  added_ms: number;
  disabled: boolean;
}

export interface DaemonRecord {
  label: string;
  added_ms: number;
  revoked: boolean;
}

export interface AccountView {
  id: string;
  email_hint: string;
  created_ms: number;
  revoked: boolean;
  has_password: boolean;
}

/// The administrative half.
///
/// Every one of these is `Caller::Admin` at the server and answers **404** to
/// anybody else — not 401, because the administrative surface does not exist for
/// them. A client that hid these would be a courtesy; the server refusing them
/// is the gate.
export const admin = {
  settings: () => get<SettingsView>("settings"),
  /// A partial update: only the fields present are written.
  ///
  /// **A blank secret must be omitted, not sent empty.** The page never shows a
  /// stored key, so an empty field means "unchanged" — sending it would clear
  /// the mail key every time somebody renamed the site.
  saveSettings: (patch: Record<string, unknown>) =>
    post<SettingsView>("settings", patch),

  owners: () => get<OwnerRecord[]>("owners"),
  addOwner: (login: string, repos: string[]) =>
    post<OwnerRecord[]>("owners", { login, repos }),

  repos: () => get<RepoRecord[]>("repos"),
  /// `anyway` forces a repository no machine has offered. The server answers
  /// **409** without it, which is a refusal to be overridden rather than an
  /// error — the daemon may simply not have polled yet.
  addRepo: (name: string, anyway: boolean) =>
    post<RepoRecord[]>("repos", { name, anyway }),

  daemons: () => get<DaemonRecord[]>("daemons"),
  /// **The response is the only copy of the key.** The volume holds a hash.
  mintDaemon: (label: string) =>
    post<{ label: string; key: string }>("daemons", { label }),

  accounts: () => get<AccountView[]>("accounts"),

  /// Revoke a record. Kept and marked, never deleted.
  revoke: <T>(list: "owners" | "repos" | "daemons" | "accounts", id: string) =>
    post<T>(`${list}/${encodeURIComponent(id)}/revoke`, {}),
};

/// Where the wizard has got to.
///
/// **The server says which step, rather than the client inferring it.** The
/// rendered pages decided from three things at once — whether the server was
/// claimed, whether an address was set, and whether this browser held the setup
/// token — and a client cannot see the third at all.
export interface SetupState {
  step: "code" | "admin";
  base_url: string;
  min_password: number;
}

/// Claiming the server. Reachable with no credential, because it is how the
/// first one comes to exist.
export const setup = {
  /// Throws with status 404 once the server is claimed: the wizard stops
  /// existing rather than refusing, so a stranger cannot tell a claimed server
  /// from one that never had it.
  state: () => get<SetupState>("setup"),
  /// Step one. The address is an environment variable, so there is nothing to
  /// mistype here and nothing that could burn the one code the operator has.
  spendCode: (code: string) => post<{ step: string }>("setup/code", { code }),
  /// Step two, bound to the browser that spent the code by a cookie the server
  /// set. Without that binding, whoever arrived next could set their own
  /// password and own the server.
  claim: (login: string, password: string) =>
    post<{ claimed: boolean }>("setup/admin", { login, password }),
};
