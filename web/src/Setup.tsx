import { useEffect, useState } from "react";
import { ApiError, setup, type SetupState } from "./api";

/// Claiming the server.
///
/// **The one flow that runs before anybody can sign in**, and the one that hands
/// over ownership — so it is the riskiest thing in this interface. If it breaks,
/// a fresh volume is unclaimable and the recovery is deleting `admin.json` from
/// the disk. Everything else here can be fixed by signing in again.
///
/// Two steps, and the order is forced rather than chosen: the address decides
/// whether session cookies carry `Secure`, so it has to be settled before a
/// password is typed, or the first credential this server ever sees might travel
/// without it.
export function Setup({ onClaimed }: { onClaimed: () => void }) {
  const [state, setState] = useState<SetupState | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    setup
      .state()
      .then(setState)
      // A 404 means the server is already claimed, which is not an error — the
      // wizard simply does not exist any more.
      .catch(() => onClaimed());
  }, [onClaimed]);

  if (!state) return null;

  if (state.step === "code") {
    return (
      <>
        <h1>Set up this server</h1>
        <p>
          This server has not been claimed. Whoever claims it administers it, so
          the code below is printed in the container&apos;s log — being able to
          read that log is the proof.
        </p>
        {error && <p className="note">{error}</p>}
        <form
          onSubmit={(e) => {
            e.preventDefault();
            const data = new FormData(e.currentTarget);
            setBusy(true);
            setError(null);
            setup
              .spendCode(
                String(data.get("code") ?? ""),
                String(data.get("base_url") ?? ""),
              )
              .then(() => setup.state().then(setState))
              .catch((x: ApiError) => setError(x.message))
              .finally(() => setBusy(false));
          }}
        >
          <label htmlFor="code">The claim code from the log</label>
          <input
            id="code"
            name="code"
            required
            autoComplete="off"
            placeholder="ABC-1234"
          />

          <label htmlFor="base_url">The address people reach this at</label>
          <input
            id="base_url"
            name="base_url"
            required
            autoComplete="off"
            defaultValue={state.base_url}
            placeholder="https://specs.example.com"
          />
          {/* **Says what it decided rather than asking.** Whether cookies carry
              `Secure` is derived from the address — "is this a private network"
              is a question people answer wrong, and answering it wrong drops
              `Secure` from every session cookie without a word. */}
          <p className="meta">
            It must start <code>https://</code>, because a sign-in link is a
            credential in a URL. A private address is allowed for trying it
            locally, and cookies will not be marked <code>Secure</code> there.
          </p>

          <button type="submit" disabled={busy}>
            Continue
          </button>
        </form>
      </>
    );
  }

  return (
    <>
      <h1>Who administers this?</h1>
      <p>
        Choose an email address and a password. This account administers this
        server: it reviews requests, decides what the public site collects, and
        holds the keys. <strong>There is no second one.</strong>
      </p>
      {error && <p className="note">{error}</p>}
      <form
        onSubmit={(e) => {
          e.preventDefault();
          const data = new FormData(e.currentTarget);
          setBusy(true);
          setError(null);
          setup
            .claim(
              String(data.get("login") ?? ""),
              String(data.get("password") ?? ""),
            )
            .then(() => onClaimed())
            .catch((x: ApiError) => setError(x.message))
            .finally(() => setBusy(false));
        }}
      >
        <label htmlFor="login">Email</label>
        <input
          id="login"
          name="login"
          type="email"
          required
          autoCapitalize="off"
          autoCorrect="off"
          spellCheck={false}
          autoComplete="username"
        />

        <label htmlFor="password">Password</label>
        <input
          id="password"
          name="password"
          type="password"
          required
          minLength={state.min_password}
          autoComplete="new-password"
        />
        <p className="meta">
          At least {state.min_password} characters. It is stored hashed and
          cannot be recovered — if you lose it, delete <code>admin.json</code>{" "}
          from the volume and claim the server again.
        </p>

        <button type="submit" disabled={busy}>
          Claim it
        </button>
      </form>
    </>
  );
}
