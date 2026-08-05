import { useEffect, useState } from "react";
import { ApiError, setup, type SetupState } from "./api";
import { useStrings } from "./strings";

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
  const s = useStrings();
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
        <h1>{s.setup_code_heading}</h1>
        <p>{s.setup_code_intro}</p>
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
          <label htmlFor="code">{s.setup_code_label}</label>
          {/* The placeholder is the *shape* of a claim code, not a word — three
              letters, a dash, four digits. Translating it would make it stop
              describing what the operator is about to paste. */}
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
              could burn the one claim code the operator has.

              Split around the URL rather than formatted with it: the catalogue
              carries no placeholders, and the URL is never translated. */}
          <p className="meta">
            {s.setup_base_url_before}
            <code>{state.base_url}</code>
            {s.setup_base_url_after}
          </p>

          <button type="submit" disabled={busy}>
            {s.setup_continue}
          </button>
        </form>
      </>
    );
  }

  return (
    <>
      <h1>{s.setup_admin_heading}</h1>
      <p>
        {s.setup_admin_intro}
        <strong>{s.setup_admin_intro_strong}</strong>
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
        <label htmlFor="login">{s.setup_email_label}</label>
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

        <label htmlFor="password">{s.setup_password_label}</label>
        <input
          id="password"
          name="password"
          type="password"
          required
          minLength={state.min_password}
          autoComplete="new-password"
        />
        {/* **Two interpolations and a plural, in one sentence.** The catalogue
            holds no placeholders, so the sentence arrives as four pieces and the
            renderer puts the minimum and the filename between them.

            The plural is not decoration: French "caractère" agrees in number
            with the count, and `min_password` is configured on the server, so a
            deployment can legitimately set it to 1. English happens not to care
            about the difference at this position, which is exactly why picking
            the wrong field would have shipped unnoticed until somebody set it to
            one. `admin.json` is a filename and stays as it is. */}
        <p className="meta">
          {s.setup_min_password_before}
          {state.min_password}
          {state.min_password === 1
            ? s.setup_min_password_chars_one
            : s.setup_min_password_chars}
          {s.setup_min_password_after}
          <code>admin.json</code>
          {s.setup_min_password_tail}
        </p>

        <button type="submit" disabled={busy}>
          {s.setup_claim}
        </button>
      </form>
    </>
  );
}
