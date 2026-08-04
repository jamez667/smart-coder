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
