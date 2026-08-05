import { execFileSync } from "node:child_process";
import { expect, test } from "@playwright/test";

// Claiming a server, through the interface, in a browser.
//
// **This is the riskiest flow in the product and the one with no fallback.** If
// it breaks, a fresh volume is unclaimable and the recovery is deleting
// `admin.json` from the disk. Everything else can be fixed by signing in again.
//
// It gets its own container rather than sharing the run's: the global setup
// claims that one, and a claimed server has no wizard to test.

const NAME = "sc-e2e-setup";
const PORT = "8798";

function docker(...args: string[]): string {
  return execFileSync("docker", args, { encoding: "utf-8" });
}

let base: string;

test.beforeAll(() => {
  try {
    docker("rm", "-f", NAME);
  } catch {
    // Not running.
  }
  const key = Array.from({ length: 32 }, () =>
    Math.floor(Math.random() * 256)
      .toString(16)
      .padStart(2, "0"),
  ).join("");
  base = `http://127.0.0.1:${PORT}`;
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
    // **Unclaimed, but configured.** The address is an environment variable
    // now, so a fresh volume has one before anybody reaches the wizard — what
    // the wizard establishes is who owns the server, and nothing else.
    process.env.SC_IMAGE ?? "sc-server:local",
  );
});

test.afterAll(() => {
  try {
    docker("rm", "-f", NAME);
  } catch {
    // Already gone.
  }
});

test("a fresh volume is claimed through the interface", async ({ page }) => {
  // Wait for it, then read the code it logged. The code is in the clear because
  // it has to be — its value is bounded by time rather than by the log's
  // audience.
  let code: string | undefined;
  for (let i = 0; i < 80 && !code; i++) {
    code = /"code":"([A-Z0-9-]+)"/.exec(docker("logs", NAME))?.[1];
    if (!code) await new Promise((r) => setTimeout(r, 250));
  }
  expect(code, "the server logged a claim code").toBeTruthy();

  await page.goto(`${base}/setup`);
  await expect(page.getByRole("heading", { level: 1 })).toHaveText(
    "Set up this server",
  );

  // **A wrong code is refused and does not advance.** There is no address to
  // mistype any more — it comes from the stack, and the server refuses to start
  // without a valid one.
  await page.locator("#code").fill("XYZ-9999");
  await page.getByRole("button", { name: "Continue" }).click();
  await expect(page.locator(".note")).toBeVisible();
  await expect(page.getByRole("heading", { level: 1 })).toHaveText(
    "Set up this server",
    { timeout: 5000 },
  );

  // The real one.
  await page.locator("#code").fill(code!);
  await page.getByRole("button", { name: "Continue" }).click();

  // Step two: the credential that will own this.
  await expect(page.getByRole("heading", { level: 1 })).toHaveText(
    "Who administers this?",
  );
  await page.locator("#login").fill("harness@example.test");
  await page.locator("#password").fill("correct-horse-battery");
  await page.getByRole("button", { name: "Claim it" }).click();

  // Claimed, and signed in already — they just chose the credential, so asking
  // for it again immediately would be ceremony.
  await page.waitForURL(`${base}/review`, { timeout: 10000 });
  const me = await page.evaluate(async () => {
    const r = await fetch("/api/v1/ui/me", { credentials: "same-origin" });
    return (await r.json()) as { role: string };
  });
  expect(me.role).toBe("administrator");

  // And the wizard is gone: 404, not a refusal, so a stranger cannot tell a
  // claimed server from one that never had it.
  const gone = await page.evaluate(async () => {
    const r = await fetch("/api/v1/ui/setup", { credentials: "same-origin" });
    return r.status;
  });
  expect(gone).toBe(404);
});
