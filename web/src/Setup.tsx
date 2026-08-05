import { useEffect, useState } from "react";
import { ApiError, setup, type SetupState } from "./api";

/// Claiming the server.
///
/// **The one flow that runs before anybody can sign in**, and the one that hands
/// over ownership — so it is the riskiest thing in this interface. If it breaks,
/// a fresh volume is unclaimable and the recovery is deleting `admin.json` from
/// the disk. Everything else here can be fixed by signing in again.
///
/// Two steps: spend the code, then choose the credential. They are separate so
/// the second is bound to the browser that spent the first — without that,
/// everything past step one is guarded only by the server being unclaimed, and
/// whoever arrives next sets their own password and owns it.
///
/// The address used to be asked for here, which is what forced the ordering. It
/// is an environment variable now, so it is settled before anybody arrives.
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
              .spendCode(String(data.get("code") ?? ""))
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

          {/* **The address is not asked for.** It is an environment variable and
              the server refuses to start without a valid one, so by the time
              anybody reaches this it is settled — and there is no typo left that
              could burn the one claim code the operator has. */}
          <p className="meta">
            This server answers at <code>{state.base_url}</code>, which is set
            where the container is configured.
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
