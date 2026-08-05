import { expect, test } from "@playwright/test";
import { signInWithPassword } from "./fixture";

// The administrative pages.
//
// **These replace `every_administrative_page_is_linked_from_the_surface`**, the
// N×N check that caught three separate unlinked-page bugs in this project. Its
// doc said why: *"Four of these were built, tested, and reachable only by
// somebody who already knew the URL — a test asks for a route directly, which is
// exactly what a person cannot do."*
//
// A client-rendered surface has no `href`s in the response body to walk, so the
// check moves here: open the menu a person opens, click what it offers, and
// assert the page arrived.

const base = () => process.env.SC_BASE_URL!;

let session: string | undefined;

test.beforeEach(async ({ context, page }) => {
  session ??= await signInWithPassword(
    base(),
    process.env.SC_ADMIN_LOGIN!,
    process.env.SC_ADMIN_PASSWORD!,
  );
  await context.addCookies([
    {
      name: "sc_device",
      value: session,
      url: base(),
      httpOnly: true,
      sameSite: "Strict",
    },
  ]);
  await page.goto("/");

  // **Assert the premise before testing anything built on it.** Every one of
  // these pages is gated on `me.can.administer`, so a session that is not the
  // administrator renders the review list instead — and the failure reads as
  // "the heading is wrong", which is a long way from "you are not signed in as
  // who you think".
  //
  // This cost a CI round trip: four tests failed with `Received: "Requests"`
  // and none of them said why.
  const me = await page.evaluate(async () => {
    const r = await fetch("/api/v1/ui/me", { credentials: "same-origin" });
    return (await r.json()) as { role: string; can: { administer: boolean } };
  });
  if (!me.can.administer) {
    // Name the two things that could have gone wrong, so one run answers it:
    // either the sign-in did not return a session, or the browser declined to
    // send it back (a `Secure` cookie over plain HTTP is the usual reason).
    const cookies = await context.cookies();
    const sent = cookies.map((c) => `${c.name}(secure=${c.secure})`).join(", ");
    throw new Error(
      `signed in as ${me.role}, not the administrator.` +
        ` base=${base()} session=${session?.slice(0, 8)}… cookies=[${sent}]`,
    );
  }
});

test("every administrative page is reachable from the menu", async ({
  page,
}) => {
  // The replacement for the linkage test. Each entry is clicked rather than
  // navigated to, because a link nobody can click is the bug being guarded
  // against.
  const pages = [
    ["Settings", "Settings"],
    ["Repositories", "Repositories"],
    ["Owners", "Owners"],
    ["Machines", "Machines"],
    ["Who can file", "Who can file"],
  ];
  for (const [label, heading] of pages) {
    // **Opened only when shut.** A `<details>` toggles, so clicking the summary
    // unconditionally closes the menu the previous navigation left open — and
    // the failure reads as "the link does not exist" rather than "the menu is
    // shut", which is a long way from the cause.
    const menu = page.locator("details.acct");
    // **`open` is a boolean attribute, so its value is the empty string** —
    // which is falsy, so testing the value re-clicks the summary and closes the
    // menu that was already open. Ask whether the attribute is there at all.
    const isOpen = await menu.evaluate((d) => (d as HTMLDetailsElement).open);
    if (!isOpen) {
      await menu.locator("> summary").click();
    }
    await page.getByRole("link", { name: label, exact: true }).click();
    await expect(page.getByRole("heading", { level: 1 })).toHaveText(heading);
  }
});

test("the settings page holds no secret at all", async ({ page }) => {
  // **There is nothing here to leak any more.** Every secret is an environment
  // variable: the mail key, the screening key, the address. The page says where
  // they live rather than offering a field that would silently do nothing.
  //
  // This replaces a test that checked the fields were blank and of type
  // password — the stronger property is that they do not exist.
  await page.goto("/settings");
  const seen = await page.evaluate(() => ({
    path: window.location.pathname,
    h1: document.querySelector("h1")?.textContent,
  }));
  expect(seen.h1, `at ${seen.path} the interface drew "${seen.h1}"`).toBe(
    "Settings",
  );
  for (const gone of ["#mail_key", "#screen_key", "#base_url", "#site_name"]) {
    await expect(page.locator(gone)).toHaveCount(0);
  }
  // And it says where they are instead.
  await expect(page.getByText("Set in the stack, not here")).toBeVisible();
});

test("a minted machine key is shown once, in the response and nowhere else", async ({
  page,
}) => {
  // The key exists only in this response — the volume holds a hash. If the page
  // did not show it here, it would be unrecoverable.
  await page.goto("/daemons");
  await page.locator("#label").fill("harness-machine");
  await page.getByRole("button", { name: "Mint a key" }).click();

  const shown = page.locator(".note pre");
  await expect(shown).toBeVisible();
  const key = (await shown.textContent())?.trim() ?? "";
  expect(key.length, "a real key came back").toBeGreaterThan(20);

  // Reloading does not show it again: there is nothing to show.
  await page.reload();
  await expect(page.locator(".note pre")).toHaveCount(0);
});

test("a repository nothing serves is refused, and the refusal is overridable", async ({
  page,
}) => {
  // Naming one no machine offers produces a queue that never drains. Refused
  // rather than silently accepted — and overridable, because the daemon may
  // simply not have polled yet.
  await page.goto("/repos");
  await page.locator("#name").fill("nothing-serves-this");
  await page.getByRole("button", { name: "Enable", exact: true }).click();

  const warning = page.locator(".note");
  await expect(warning).toContainText("No machine has offered");
  await page.getByRole("button", { name: "Enable it anyway" }).click();
  await expect(page.locator(".item").filter({ hasText: "nothing-serves-this" })).toBeVisible();
});
