import { expect, test } from "@playwright/test";
import { signInWithPassword } from "./fixture";

// One test, deliberately.
//
// **This suite is not here to cover the surface** — the Rust tests do that, in
// process, in two seconds, and they can drive time and inspect the volume in
// ways a browser cannot. What only a real browser can prove is the narrow class
// of failure that has no status code: a stylesheet the CSP refuses, a bundle
// that never executes, a route the client cannot reach. Every such bug found so
// far showed up on the very first page load, so a suite of twenty-nine paid a
// Docker build to learn the same thing twenty-nine times.
//
// So: load the application as a stranger, sign in as the administrator, and
// reach an authenticated view. If script, styles, the API and routing all work,
// this passes; if any of them is broken, it fails on the first assertion that
// touches it.
//
// The rule for adding to this file: a test belongs here only if it **cannot**
// be written in Rust. Anything about status codes, gating, or what a caller may
// see is an in-process test — that is where the other twenty-eight went.
test("the interface loads, styles itself, and signs somebody in", async ({
  page,
  context,
}) => {
  const problems: string[] = [];
  page.on("console", (m) => {
    if (m.type() === "error") problems.push(m.text());
  });
  page.on("pageerror", (e) => problems.push(e.message));

  // --- a stranger's first load ---------------------------------------------
  await page.goto("/");
  await expect(page.getByRole("heading", { level: 1 })).toBeVisible();

  // The bundle ran. Without this the page is the served shell and every
  // assertion below would fail for the same reason, so it is worth naming.
  await expect(page.locator("#root")).not.toBeEmpty();

  // The stylesheet loaded and was not refused. An unstyled page is not an error
  // status — it looks broken and passes every test that reads a status code.
  const bg = await page.evaluate(
    () => getComputedStyle(document.body).backgroundImage,
  );
  expect(bg, "the body carries the design's gradient").not.toBe("none");

  // --- signing in ----------------------------------------------------------
  const base = process.env.SC_BASE_URL ?? "";
  const session = await signInWithPassword(
    base,
    process.env.SC_ADMIN_LOGIN ?? "",
    process.env.SC_ADMIN_PASSWORD ?? "",
  );
  await context.addCookies([
    {
      name: "sc_device",
      value: session,
      url: base,
      httpOnly: true,
      sameSite: "Strict",
    },
  ]);

  // --- an authenticated view -----------------------------------------------
  // `/settings` serves the same document to everybody; what makes this an
  // administrator's page is the JSON behind it. Reaching a settings heading
  // therefore proves the whole chain: document, bundle, cookie, API, and the
  // client routing on a path it was loaded at rather than navigated to.
  await page.goto("/settings");
  await expect(
    page.getByRole("heading", { name: /settings/i }),
  ).toBeVisible();

  expect(problems, `console errors: ${problems.join(" | ")}`).toEqual([]);
});
