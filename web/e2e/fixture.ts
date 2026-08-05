import { execFileSync } from "node:child_process";
import { hostname } from "node:os";

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
const NAME = process.env.SC_CONTAINER ?? "sc-e2e";

function docker(...args: string[]): string {
  return execFileSync("docker", args, { encoding: "utf-8" });
}

/// Are the tests themselves running inside a container?
///
/// **This decides how the server is reachable, and getting it wrong is a failure
/// that only appears in CI.** On a developer's machine the tests run on the
/// host, so a published port on `127.0.0.1` works. In CI they run in a
/// Playwright container driving the Docker socket, so the server it starts is a
/// *sibling*: `127.0.0.1` is the test container, not the server, and a published
/// port is not reachable from there at all.
///
/// The answer is to address the sibling by container name on the shared network.
function inContainer(): boolean {
  return !!process.env.CI || /^[0-9a-f]{12}$/.test(hostname());
}

export interface Server {
  /// Where to reach it.
  ///
  /// **Ask for this rather than assuming a port.** It differs between a
  /// developer's machine and CI, and a spec that hardcoded one passed locally
  /// and failed everywhere else.
  base: string;
  /// The administrator's credential, chosen during the claim.
  login: string;
  password: string;
  /// File a request and draft it, so there is something to review.
  fileAndDraft(text: string): Promise<string>;
  stop(): void;
}

/// Stand up a scratch server and claim it.
export async function server(): Promise<Server> {
  try {
    docker("rm", "-f", NAME);
  } catch {
    // Not running. Fine.
  }
  // **Always built, never reused.** This was "build it if it is missing", which
  // is wrong on a host that keeps images between runs: the tag survives, so
  // every run after the first tested whatever was built the first time. Six CI
  // pipelines ran against a stale bundle while the source said otherwise, and
  // the symptom was a routing bug that did not exist.
  //
  // The layers are cached by Docker, so rebuilding an unchanged tree is cheap
  // and rebuilding a changed one is the entire point.
  execFileSync("docker", ["build", "-t", IMAGE, ".."], { stdio: "inherit" });

  const key = Array.from({ length: 32 }, () =>
    Math.floor(Math.random() * 256)
      .toString(16)
      .padStart(2, "0"),
  ).join("");

  const sibling = inContainer();
  // **Join the server to the network this container is already on**, rather than
  // guessing what it is called. Woodpecker names it after the workflow, so it
  // differs per run and there is nothing stable to hardcode — and a container
  // name only resolves between containers that share a network.
  //
  // Asking Docker which network we are on is the one reliable answer: if this is
  // a container, the daemon knows where it is attached.
  let network: string | undefined;
  if (sibling) {
    try {
      // **Separated, because a container can be on more than one.** The first
      // attempt used `{{$k}}` with no delimiter and got `bridgewp_01k..._default`
      // — two names run together, which Docker then reported as one network it
      // could not find. A newline between them is the difference between reading
      // the answer and reading a concatenation of answers.
      const attached = docker(
        "inspect",
        "-f",
        "{{range $k, $v := .NetworkSettings.Networks}}{{$k}}\n{{end}}",
        hostname(),
      )
        .split("\n")
        .map((n) => n.trim())
        .filter(Boolean);
      // The workflow's own network rather than the default bridge: that is the
      // one the other steps and their siblings can resolve names on.
      network = attached.find((n) => n !== "bridge") ?? attached[0];
    } catch {
      // Not a container the daemon knows, which means the detection above was
      // wrong. Fall through to a published port and let `waitFor` say so.
    }
  }

  // The base URL is not only how the tests reach it — the server validates its
  // own cookies and builds its links from it, so this has to be the address that
  // actually works from where the tests are.
  //
  // **By IP, not by container name.** `http://sc-e2e:8420` is refused at boot,
  // and rightly: `check_base_url` demands `https://` unless the host is private,
  // because a sign-in link is a credential in a URL — and a container name is
  // neither https nor private. That rule is not worth weakening for a test.
  //
  // Docker hands out `172.16-31.x` on its networks, which the rule already
  // accepts — but the address is not known until the container exists, and the
  // container will not boot without being told it. `startOnNetwork` breaks that
  // circle: start, read the address the daemon assigned, restart with it. Two
  // starts of something that boots in milliseconds, against either weakening the
  // check or pre-allocating a subnet Woodpecker did not create.
  const base = network
    ? `http://${startOnNetwork(network, env(key, "http://127.0.0.1:8420"))}:8420`
    : "http://127.0.0.1:8799";

  docker(
    "run",
    "-d",
    "--name",
    NAME,
    ...(network ? ["--network", network] : ["-p", "8799:8420"]),
    ...env(key, base),
    IMAGE,
  );

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
  if (!setup) throw new Error(`no setup cookie (${step1.status})`);

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
      //
      // Filed by the administrator rather than by a magic-link filer: the mail
      // provider here is not real and the console mailer was removed, so there
      // is no way to receive a link. A request filed from the private surface
      // reaches the review gate identically.
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
      // The wire calls it `item`, not `work` — the envelope carries a `type` and
      // the payload sits beside it.
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
    },
  };
}

/// Start the container, note the address the daemon gave it, and remove it.
///
/// The server refuses to boot without a valid base URL, and its base URL is an
/// address that does not exist until it does. This breaks that circle the cheap
/// way rather than by making the server less strict.
function startOnNetwork(network: string, envArgs: string[]): string {
  docker("run", "-d", "--name", NAME, "--network", network, ...envArgs, IMAGE);
  const ip = docker(
    "inspect",
    "-f",
    "{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}",
    NAME,
  ).trim();
  // Removed rather than left running: the caller starts it again with the
  // address it now knows, and a half-configured server listening on the
  // network is a thing a later test could reach by accident.
  docker("rm", "-f", NAME);
  if (!ip) throw new Error(`no address was assigned on ${network}`);
  return ip;
}

/// The server's environment, all of it in one place so the two starts cannot
/// drift apart.
function env(key: string, base: string): string[] {
  return [
    "-e", `SC_SERVER_SECRET_KEY=${key}`,
    "-e", `SC_SERVER_DAEMON_KEYS=harness:${DAEMON_KEY}`,
    "-e", `SC_SERVER_PUBLIC_BASE_URL=${base}`,
    "-e", "SC_SERVER_PUBLIC_REPOS=intake",
    "-e", "SC_SERVER_MAIL_PROVIDER=brevo",
    "-e", "SC_SERVER_MAIL_KEY=xkeysib-harness-not-real-00000000",
    "-e", "SC_SERVER_MAIL_FROM=noreply@example.test",
  ];
}

async function waitFor(base: string): Promise<void> {
  for (let i = 0; i < 80; i++) {
    try {
      const res = await fetch(base, { redirect: "manual" });
      if (res.status < 500) return;
    } catch {
      // Not listening yet.
    }
    await new Promise((r) => setTimeout(r, 250));
  }
  // **Say why, rather than only that.** "It never came up" is true of a
  // container that crashed, one on an unreachable network, and one still
  // compiling — three different fixes, and the message distinguishes none of
  // them. This has cost several CI round trips already.
  let detail = "";
  try {
    const state = docker("inspect", "-f", "{{.State.Status}} {{.State.ExitCode}}", NAME).trim();
    const nets = docker(
      "inspect",
      "-f",
      "{{range $k, $v := .NetworkSettings.Networks}}{{$k}} {{end}}",
      NAME,
    ).trim();
    const logs = docker("logs", "--tail", "20", NAME);
    detail = `
  container: ${state}
  networks: ${nets}
  logs:
${logs}`;
  } catch (e) {
    detail = `
  (could not inspect the container: ${String(e).slice(0, 200)})`;
  }
  throw new Error(`the server never came up at ${base}${detail}`);
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
  ...Array.from(
    { length: 40 },
    (_, i) => `Paragraph ${i + 1} of the drafted specification.`,
  ),
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
  const session = /sc_device=([a-f0-9]+)/.exec(
    res.headers.get("set-cookie") ?? "",
  )?.[1];
  if (!session) throw new Error(`the password sign-in failed: ${res.status}`);
  return session;
}
