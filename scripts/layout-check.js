// Measure the running server's header and report what is misaligned.
//
//   node scripts/layout-check.js [url]
//
// **Why this exists.** Four rounds of "the header still is not aligned" went by
// with me adjusting a property, rebuilding, and asking the user to look. That is
// the wrong loop: the browser knows the answer exactly, and asking a person to
// eyeball pixels is both slower and less reliable than reading them back.
//
// **No new dependency.** This drives the Chrome that is already installed, via
// its remote debugging port and a WebSocket. Playwright would mean npm, a
// package.json, a lockfile and a browser download in a repository that
// deliberately has no JS toolchain — a large amount of machinery to answer
// "is the button 32px tall".
//
// It asserts *relationships*, not fixed numbers: the items in the nav row share
// a centre line, the knob sits inside its track, nothing overflows. A test
// pinned to 32px would have to be edited every time the design moves; these
// hold for any design that is actually aligned.
//
// **Not part of `check.sh`.** That gate is offline and deterministic; this needs
// a running server and a browser on the machine, and a gate that cannot run
// everywhere is a gate somebody disables. Run it after a change to the public
// surface's layout — it takes a couple of seconds and answers the question the
// gate cannot.

const { spawn } = require('node:child_process');
const http = require('node:http');
const os = require('node:os');
const path = require('node:path');
const fs = require('node:fs');

const URL_UNDER_TEST = process.argv[2] || 'http://localhost:8430/';
const PORT = 9333;

const CHROME = [
  'C:/Program Files/Google/Chrome/Application/chrome.exe',
  'C:/Program Files (x86)/Google/Chrome/Application/chrome.exe',
  'C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe',
].find((p) => fs.existsSync(p));

if (!CHROME) {
  console.error('no Chrome or Edge found; skipping layout check');
  process.exit(0);
}

const get = (p) =>
  new Promise((resolve, reject) => {
    http
      .get({ host: '127.0.0.1', port: PORT, path: p }, (res) => {
        let body = '';
        res.on('data', (c) => (body += c));
        res.on('end', () => {
          try {
            resolve(JSON.parse(body));
          } catch (e) {
            reject(e);
          }
        });
      })
      .on('error', reject);
  });

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// What the page is asked to measure. Runs inside the browser.
const PROBE = `(() => {
  const box = (sel) => {
    const el = document.querySelector(sel);
    if (!el) return null;
    const r = el.getBoundingClientRect();
    return { sel, x: r.x, y: r.y, w: r.width, h: r.height,
             top: r.top, bottom: r.bottom, mid: r.top + r.height / 2 };
  };
  // The pill that is actually visible — one of the pair is display:none.
  const pill = [...document.querySelectorAll('.theme')]
    .find((el) => el.getBoundingClientRect().width > 0);
  const pillBox = pill ? (() => {
    const r = pill.getBoundingClientRect();
    return { sel: '.theme', x: r.x, y: r.y, w: r.width, h: r.height,
             top: r.top, bottom: r.bottom, mid: r.top + r.height / 2 };
  })() : null;
  const knob = pill ? (() => {
    const r = pill.querySelector('.knob').getBoundingClientRect();
    return { sel: '.knob', x: r.x, y: r.y, w: r.width, h: r.height,
             top: r.top, bottom: r.bottom, mid: r.top + r.height / 2 };
  })() : null;
  return JSON.stringify({
    pill: pillBox,
    knob,
    select: box('.lang select'),
    // The **visible** button. ".controls .btn" also matches the no-script
    // fallback link, which is display:none at 0x0 — measuring that reported a
    // perfectly aligned zero-sized box while the real button sat on another row.
    // (No backticks in this comment: it lives inside a template literal.)
    signin: (() => {
      const el = [...document.querySelectorAll('.controls .btn')]
        .find((e) => e.getBoundingClientRect().width > 0);
      if (!el) return null;
      const r = el.getBoundingClientRect();
      return { sel: '.btn', x: r.x, y: r.y, w: r.width, h: r.height,
               top: r.top, bottom: r.bottom, mid: r.top + r.height / 2 };
    })(),
    controls: box('.controls'),
    main: box('main'),
    barInner: box('.bar-inner'),
    docWidth: document.documentElement.clientWidth,
    bodyScrollWidth: document.body.scrollWidth,
  });
})()`;

async function main() {
  const profile = fs.mkdtempSync(path.join(os.tmpdir(), 'sc-layout-'));
  const chrome = spawn(
    CHROME,
    [
      '--headless=new',
      `--remote-debugging-port=${PORT}`,
      `--user-data-dir=${profile}`,
      '--no-first-run',
      '--no-default-browser-check',
      '--window-size=1280,900',
      URL_UNDER_TEST,
    ],
    { stdio: 'ignore' },
  );

  let target = null;
  for (let i = 0; i < 40 && !target; i++) {
    await sleep(250);
    try {
      const list = await get('/json/list');
      target = list.find((t) => t.type === 'page' && t.webSocketDebuggerUrl);
    } catch {
      /* not up yet */
    }
  }
  if (!target) {
    chrome.kill();
    console.error('could not reach the browser');
    process.exit(1);
  }

  // Give the page a moment to lay out and the fonts to apply — a measurement
  // taken mid-load reports the fallback face's metrics, which is a different
  // page from the one that ends up on screen.
  await sleep(1200);

  const measured = await evaluate(target.webSocketDebuggerUrl, PROBE);
  chrome.kill();
  try {
    fs.rmSync(profile, { recursive: true, force: true });
  } catch {
    /* the browser may still hold a handle; it is a temp dir either way */
  }

  report(JSON.parse(measured));
}

function evaluate(wsUrl, expression) {
  // A minimal websocket client. The protocol needs one text frame out and one
  // in, which is far less code than taking on a dependency for it.
  return new Promise((resolve, reject) => {
    const net = require('node:net');
    const crypto = require('node:crypto');
    const u = new global.URL(wsUrl);
    const key = crypto.randomBytes(16).toString('base64');
    const sock = net.connect(Number(u.port), u.hostname, () => {
      sock.write(
        `GET ${u.pathname} HTTP/1.1\r\nHost: ${u.host}\r\nUpgrade: websocket\r\n` +
          `Connection: Upgrade\r\nSec-WebSocket-Key: ${key}\r\n` +
          `Sec-WebSocket-Version: 13\r\n\r\n`,
      );
    });

    let handshake = false;
    let buf = Buffer.alloc(0);
    sock.on('data', (chunk) => {
      buf = Buffer.concat([buf, chunk]);
      if (!handshake) {
        const end = buf.indexOf('\r\n\r\n');
        if (end < 0) return;
        handshake = true;
        buf = buf.subarray(end + 4);
        sock.write(
          frame(
            JSON.stringify({
              id: 1,
              method: 'Runtime.evaluate',
              params: { expression, returnByValue: true, awaitPromise: true },
            }),
          ),
        );
      }
      const msg = unframe(buf);
      if (!msg) return;
      try {
        const parsed = JSON.parse(msg);
        if (parsed.id === 1) {
          sock.destroy();
          const r = parsed.result && parsed.result.result;
          if (!r) return reject(new Error('no result'));
          resolve(r.value);
        }
      } catch (e) {
        reject(e);
      }
    });
    sock.on('error', reject);
  });
}

function frame(text) {
  const payload = Buffer.from(text);
  const mask = require('node:crypto').randomBytes(4);
  let header;
  if (payload.length < 126) {
    header = Buffer.from([0x81, 0x80 | payload.length]);
  } else {
    header = Buffer.alloc(4);
    header[0] = 0x81;
    header[1] = 0x80 | 126;
    header.writeUInt16BE(payload.length, 2);
  }
  const masked = Buffer.alloc(payload.length);
  for (let i = 0; i < payload.length; i++) masked[i] = payload[i] ^ mask[i % 4];
  return Buffer.concat([header, mask, masked]);
}

function unframe(buf) {
  if (buf.length < 2) return null;
  let len = buf[1] & 0x7f;
  let offset = 2;
  if (len === 126) {
    if (buf.length < 4) return null;
    len = buf.readUInt16BE(2);
    offset = 4;
  } else if (len === 127) {
    if (buf.length < 10) return null;
    len = Number(buf.readBigUInt64BE(2));
    offset = 10;
  }
  if (buf.length < offset + len) return null;
  return buf.subarray(offset, offset + len).toString();
}

function report(m) {
  const problems = [];
  const ok = [];
  const near = (a, b, slack, what) => {
    const d = Math.abs(a - b);
    (d <= slack ? ok : problems).push(
      `${what}: off by ${d.toFixed(2)}px (tolerance ${slack})`,
    );
  };

  if (!m.pill || !m.select || !m.signin) {
    console.error('could not find the header controls; is the server running?');
    process.exit(1);
  }

  // The nav row shares one centre line. Half a pixel of slack for subpixel
  // rounding; anything more is visible.
  near(m.pill.mid, m.select.mid, 0.5, 'theme pill vs language select, centres');
  near(m.select.mid, m.signin.mid, 0.5, 'language select vs sign in, centres');
  near(m.pill.mid, m.signin.mid, 0.5, 'theme pill vs sign in, centres');

  // The knob sits inside its track, with equal air above and below.
  if (m.knob) {
    const above = m.knob.top - m.pill.top;
    const below = m.pill.bottom - m.knob.bottom;
    near(above, below, 0.5, `knob inside the track (${above.toFixed(1)} above, ${below.toFixed(1)} below)`);
    if (above < 0 || below < 0) {
      problems.push(`knob overflows its track: ${above.toFixed(1)} above, ${below.toFixed(1)} below`);
    }
  }

  // One row, not two.
  if (m.controls && m.signin.bottom > m.controls.bottom + 0.5) {
    problems.push('a control has wrapped onto a second line');
  } else {
    ok.push('the nav is one row');
  }

  // Nothing pushes the page sideways.
  if (m.bodyScrollWidth > m.docWidth + 1) {
    problems.push(`the page scrolls sideways by ${(m.bodyScrollWidth - m.docWidth).toFixed(0)}px`);
  } else {
    ok.push('no horizontal overflow');
  }

  for (const line of ok) console.log(`  ok    ${line}`);
  for (const line of problems) console.log(`  FAIL  ${line}`);
  console.log(
    `\n  ${problems.length === 0 ? 'aligned' : `${problems.length} problem(s)`}`,
  );
  process.exit(problems.length === 0 ? 0 : 1);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
