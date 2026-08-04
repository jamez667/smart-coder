import { execFileSync } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

// A real server, claimed, with a real drafted request in it.
//
// **The browser tests are worth nothing without this.** A review test that skips
// when there is nothing to review is a test that passes on a broken build, and
// the properties it guards — that the two decisions weigh the same, that the
// controls sit below the artifact — are exactly the ones nobody notices
// breaking.
//
// It shells out to `docker` rather than driving the server in-process because
// the point is to exercise the shipped image: the same binary, the same embedded
// bundle, the same CSP.

const IMAGE = process.env.SC_IMAGE ?? "sc-server:local";
const PORT = process.env.SC_PORT ?? "8799";
const NAME = "sc-e2e";

function docker(...args: string[]): string {
  return execFileSync("docker", args, { encoding: "utf-8" });
}

export interface Server {
  base: string;
  /// The administrator's credential, chosen during the claim.
  login: string;
  password: string;
  /// File a request and draft it, so there is something to review.
  fileAndDraft(text: string): Promise<string>;
  stop(): void;
}

/// Stand up a scratch server, claim it, and file one request that has been
/// drafted and is awaiting review.
export async function server(): Promise<Server> {
  try {
    docker("rm", "-f", NAME);
  } catch {
    // Not running. Fine.
  }
  // **Build the image if it is not there.** Locally it usually is, from the
  // last `docker build`; in CI it never is, and a harness that assumed
  // otherwise would fail with "no such image" rather than testing anything.
  try {
    docker("image", "inspect", IMAGE);
  } catch {
    execFileSync("docker", ["build", "-t", IMAGE, ".."], {
      stdio: "inherit",
    });
  }
  const dir = mkdtempSync(join(tmpdir(), "sc-e2e-"));
  const key = Array.from({ length: 32 }, () =>
    Math.floor(Math.random() * 256)
      .toString(16)
      .padStart(2, "0"),
  ).join("");

  docker(
    "run",
    "-d",
    "--name",
    NAME,
    "-p",
    `${PORT}:8420`,
    "-e",
    `SC_SERVER_SECRET_KEY=${key}`,
    "-e",
    "SC_SERVER_DAEMON_KEYS=harness:0123456789abcdef0123456789abcdef",
    "-e",
    `SC_SERVER_PUBLIC_BASE_URL=http://127.0.0.1:${PORT}`,
    "-e",
    "SC_SERVER_PUBLIC_REPOS=intake",
    "-e",
    "SC_SERVER_MAIL_PROVIDER=brevo",
    "-e",
    "SC_SERVER_MAIL_KEY=xkeysib-harness-not-real-00000000",
    "-e",
    "SC_SERVER_MAIL_FROM=noreply@example.test",
    "-e",
    "SC_SERVER_UI=1",
    IMAGE,
  );

  const base = `http://127.0.0.1:${PORT}`;
  await waitFor(base);

  // The claim code is logged, in the clear, because it has to be — its value is
  // bounded by time rather than by the log's audience.
  const logs = docker("logs", NAME);
  const code = /"code":"([A-Z0-9-]+)"/.exec(logs)?.[1];
  if (!code) throw new Error("the server logged no claim code");

  const login = "harness@example.test";
  const password = "correct-horse-battery";

  // Step one: spend the code. The response sets the cookie binding the rest of
  // the wizard to this client.
  const step1 = await fetch(`${base}/setup`, {
    method: "POST",
    headers: { "Content-Type": "application/x-www-form-urlencoded" },
    body: `code=${code}&base_url=${encodeURIComponent(base)}`,
    redirect: "manual",
  });
  const setup = /sc_setup=([a-f0-9]+)/.exec(
    step1.headers.get("set-cookie") ?? "",
  )?.[1];
  if (!setup) throw new Error("no setup cookie");

  // Step two: choose the credential that owns this server.
  const step2 = await fetch(`${base}/setup/admin`, {
    method: "POST",
    headers: {
      "Content-Type": "application/x-www-form-urlencoded",
      Cookie: `sc_setup=${setup}`,
    },
    body: `login=${encodeURIComponent(login)}&password=${encodeURIComponent(password)}`,
    redirect: "manual",
  });
  if (!step2.ok) throw new Error(`the claim failed: ${step2.status}`);

  return {
    base,
    login,
    password,
    async fileAndDraft(text: string): Promise<string> {
      // **Filed and drafted through the real paths**, not written into the
      // store. A fixture that assembles a record by hand keeps passing after
      // the real path grows a rule, and the review gate is exactly where a
      // fixture must not diverge from what a request actually looks like.
      // **Filed by the administrator, not by a magic-link filer.** The mail
      // provider in this fixture is not real and the console mailer was removed,
      // so there is no way to receive a link — and inventing one would mean the
      // harness signing in by a path no user has.
      //
      // What is under test here is the review gate, and a request filed from the
      // private surface reaches it identically.
      const admin = await signInWithPassword(base, login, password);
      await fetch(`${base}/file`, {
        method: "POST",
        headers: {
          "Content-Type": "application/x-www-form-urlencoded",
          Cookie: `sc_device=${admin}`,
        },
        body: `text=${encodeURIComponent(text)}&kind=bug&repo=intake`,
        redirect: "manual",
      });

      // Now be the daemon: claim the work and post a drafted spec back.
      const poll = await fetch(`${base}/api/v1/work?repo=intake`, {
        headers: { Authorization: `Bearer ${DAEMON_KEY}` },
      });
      // The wire calls it `item`, not `work` — the envelope carries a
      // `type` and the payload sits beside it.
      const work = (await poll.json()) as { item?: { id: string } };
      const id = work.item?.id;
      if (!id) throw new Error("the daemon was offered no work");

      await fetch(`${base}/api/v1/work/${id}/drafted`, {
        method: "POST",
        headers: {
          Authorization: `Bearer ${DAEMON_KEY}`,
          "Content-Type": "application/json",
        },
        body: JSON.stringify({
          protocol: 1,
          id,
          spec: SPEC,
          artifact_dir: "specs/harness",
        }),
      });
      return id;
    },
    stop() {
      try {
        docker("rm", "-f", NAME);
      } catch {
        // Already gone.
      }
      rmSync(dir, { recursive: true, force: true });
    },
  };
}

async function waitFor(base: string): Promise<void> {
  for (let i = 0; i < 60; i++) {
    try {
      const res = await fetch(base, { redirect: "manual" });
      if (res.status < 500) return;
    } catch {
      // Not listening yet.
    }
    await new Promise((r) => setTimeout(r, 250));
  }
  throw new Error("the server never came up");
}

const DAEMON_KEY = "0123456789abcdef0123456789abcdef";

/// A spec long enough that the decision controls are genuinely below it.
///
/// The document-order property is about a phone screen: a two-line spec would
/// let the buttons sit above the fold and the test would pass while the property
/// it guards was untested.
const SPEC = [
  "# What this changes",
  "",
  ...Array.from({ length: 40 }, (_, i) => `Paragraph ${i + 1} of the drafted specification.`),
].join("\n");

/// Sign in with the password chosen at the claim.
export async function signInWithPassword(
  base: string,
  login: string,
  password: string,
): Promise<string> {
  const res = await fetch(`${base}/public/signin/password`, {
    method: "POST",
    headers: { "Content-Type": "application/x-www-form-urlencoded" },
    body: `login=${encodeURIComponent(login)}&password=${encodeURIComponent(password)}`,
    redirect: "manual",
  });
  const session = /sc_device=([a-f0-9]+)/.exec(res.headers.get("set-cookie") ?? "")?.[1];
  if (!session) throw new Error(`the password sign-in failed: ${res.status}`);
  return session;
}
