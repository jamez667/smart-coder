import { expect, test } from "@playwright/test";

// The properties that used to be asserted against rendered HTML in Rust.
//
// Each test names the in-process test it replaces, because the point is not
// "the interface works" — it is that a specific argument recorded in a spec is
// still true after the surface moved.

test.describe("the public surface", () => {
  test("renders without a console error and mounts the application", async ({
    page,
  }) => {
    // The one thing a `curl` cannot check: the CSP is present and correct on
    // paper either way, and only a browser refuses a stylesheet or a bundle.
    // An unstyled page is not an error status — it is a page that looks broken
    // and passes every test that reads a status code.
    const problems: string[] = [];
    page.on("console", (m) => {
      if (m.type() === "error") problems.push(m.text());
    });
    page.on("pageerror", (e) => problems.push(e.message));

    await page.goto("/");
    await expect(page.getByRole("heading", { level: 1 })).toBeVisible();
    expect(problems, `console errors: ${problems.join(" | ")}`).toEqual([]);
  });

  test("loads its stylesheet, so the page is styled rather than bare", async ({
    page,
  }) => {
    // Replaces nothing — this property had no test, because until now the CSS
    // was inlined in the document and could not fail to load. It can now.
    await page.goto("/");
    const bg = await page.evaluate(
      () => getComputedStyle(document.body).backgroundImage,
    );
    expect(bg, "the body carries the design's gradient").not.toBe("none");
  });

  test("offers a stranger a way in", async ({ page }) => {
    await page.goto("/");
    await page.locator("header button.btn").click();
    await expect(page.locator("dialog#signin-dialog")).toBeVisible();
    // Both ways in, which is the property `the_sign_in_dialog_carries_both_ways_in`
    // asserted in Rust: the magic link, and the password behind a disclosure.
    await expect(page.getByLabel("Email").first()).toBeVisible();
    await expect(page.getByText("Admin login")).toBeVisible();
  });

  test("never names an administrative route to a stranger", async ({ page }) => {
    // **Replaces `a_page_a_stranger_can_reach_names_no_administrative_route`.**
    // This one is a leak test, and it is the test most at risk from a single
    // bundle shipped to everybody: the interface must draw its menu from what
    // the server granted, not from what it can imagine.
    await page.goto("/");
    const html = await page.content();
    for (const route of [
      "/settings",
      "/repos",
      "/owners",
      "/daemons",
      "/accounts",
    ]) {
      expect(html, `a stranger is shown ${route}`).not.toContain(
        `href="${route}"`,
      );
    }
  });

  test("the sign-in dialog traps focus and closes on Escape", async ({
    page,
  }) => {
    // A real `<dialog>` brings this; an overlay has to reimplement it and
    // usually skips the accessibility half. Asserting it here is what keeps the
    // element from being swapped for a div later.
    await page.goto("/");
    await page.locator("header button.btn").click();
    await expect(page.locator("dialog#signin-dialog")).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(page.locator("dialog#signin-dialog")).not.toBeVisible();
  });
});
