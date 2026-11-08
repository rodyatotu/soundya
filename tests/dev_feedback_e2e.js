#!/usr/bin/env node
// The developer build's feedback mode on the real panel in Chromium (Playwright),
// with the daemon running on this machine (no speaker: an imitation of one).
//   cargo build --features dev && cp target/debug/soundya /tmp/soundya-dev
//   cargo build && node tests/dev_feedback_e2e.js /tmp/soundya-dev target/debug/soundya
// The second binary (the normal build) must have no feedback mode at all.
'use strict';
const { chromium } = require('playwright');
const { spawn, execFileSync } = require('child_process');
const fs = require('fs');
const os = require('os');
const path = require('path');
const net = require('net');

const [DEV_BIN, USER_BIN] = process.argv.slice(2);
if (!DEV_BIN) { console.error('usage: dev_feedback_e2e.js <dev binary> [normal binary]'); process.exit(2); }
const SECRET = '2-123456-E2ESECRETTOKEN';
let fails = 0;
const check = (name, ok, extra) => { console.log((ok ? '  ok   ' : '  FAIL ') + name + (ok || extra === undefined ? '' : ' — ' + extra)); fails += ok ? 0 : 1; };

function waitPort(port, ms = 20000) {
  const end = Date.now() + ms;
  return new Promise((ok, no) => {
    const tryOnce = () => {
      const s = net.connect(port, '127.0.0.1', () => { s.end(); ok(); });
      s.on('error', () => (Date.now() > end ? no(new Error('port ' + port)) : setTimeout(tryOnce, 200)));
    };
    tryOnce();
  });
}

async function start(bin, port) {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'soundya-dev-e2e-'));
  const state = path.join(tmp, 'state'), run = path.join(tmp, 'run');
  fs.mkdirSync(state); fs.mkdirSync(run);
  fs.writeFileSync(path.join(state, 'config.json'), JSON.stringify({ token: SECRET, stats: false, airplay: false, onboarded: true }));
  const d = spawn(bin, ['daemon'], { env: Object.assign({}, process.env, { SOUNDYA_DIR: state, SOUNDYA_RUN: run,
    SOUNDYA_WEB_PORT: String(port), SOUNDYA_NO_VOICE: '1', SOUNDYA_NAME: 'soundya-e2e' }), stdio: 'ignore' });
  await waitPort(port);
  return { d, tmp };
}

(async () => {
  fs.appendFileSync('/tmp/soundya.log', `e2e: login with ${SECRET}\ne2e: marker line for the feedback log\n`);
  const PORT = 18877;
  const { d } = await start(DEV_BIN, PORT);
  const browser = await chromium.launch();
  try {
    const page = await browser.newPage({ viewport: { width: 1200, height: 800 }, acceptDownloads: true });
    await page.goto(`http://127.0.0.1:${PORT}/`);
    await page.waitForFunction(() => window.__sdFeedback);
    console.log('1. the developer build');
    const status = await (await page.request.get(`http://127.0.0.1:${PORT}/api/status`)).json();
    check('status says dev and «+dev»', status.device && status.device.dev === true && /\+dev$/.test(status.device.version), JSON.stringify(status.device));
    const upd = await (await page.request.post(`http://127.0.0.1:${PORT}/api/cmd`, { data: 'update install', headers: { Origin: `http://127.0.0.1:${PORT}` } })).text();
    check('no update from GitHub', /сборка разработчика/.test(upd), upd);

    console.log('2. errors are caught from the start');
    await page.evaluate(() => { setTimeout(() => { throw new Error('e2e boom'); }, 0); fetch('/api/nothing-here'); });
    await page.waitForTimeout(500);
    const errs = await page.evaluate(() => window.__sdErrors.map((e) => e.kind + ': ' + e.text));
    check('a thrown error', errs.some((e) => /e2e boom/.test(e)), errs.join(' | '));
    check('a failed request', errs.some((e) => /^http: GET \/api\/nothing-here → 404/.test(e)), errs.join(' | '));

    console.log('3. pick an element, drag the frame, write a note');
    // A visible button of the panel to aim at.
    const target = await page.evaluateHandle(() => [...document.querySelectorAll('button')].find((b) => {
      const r = b.getBoundingClientRect();
      return r.width > 30 && r.height > 16 && r.top > 0 && r.bottom < innerHeight && r.left > 200 && !b.closest('[data-sd]');
    }));
    const r = await target.evaluate((b) => { const x = b.getBoundingClientRect(); return { x: x.left, y: x.top, w: x.width, h: x.height }; });
    await target.evaluate((b) => { window.__clicked = 0; b.addEventListener('click', () => window.__clicked++); });
    await page.keyboard.press('Alt+KeyF');
    await page.mouse.move(r.x + r.w / 2, r.y + r.h / 2);
    await page.mouse.click(r.x + r.w / 2, r.y + r.h / 2);
    check('the page did not get the click', (await page.evaluate(() => window.__clicked)) === 0);
    const card = page.locator('[data-sd] >> css=.card');
    check('the card opened', await card.isVisible());
    // The east handle: 40 px wider.
    const e = page.locator('[data-sd] >> css=.ghost .h[data-h="e"]');
    const hb = await e.boundingBox();
    await page.mouse.move(hb.x + hb.width / 2, hb.y + hb.height / 2);
    await page.mouse.down();
    await page.mouse.move(hb.x + hb.width / 2 + 40, hb.y + hb.height / 2, { steps: 5 });
    await page.mouse.up();
    const delta = await page.locator('[data-sd] >> css=.card .delta').textContent();
    check('the delta is shown', /ширина \+40 px/.test(delta), delta);
    const kindOn = await page.locator('[data-sd] >> css=.kinds button.on').textContent();
    check('dragging wider means «Увеличить»', /Увеличить/.test(kindOn), kindOn);
    await page.locator('[data-sd] >> css=.card .text').fill('кнопка мелкая');
    await page.locator('[data-sd] >> css=.card .ok').click();
    await page.waitForFunction(() => window.__sdFeedback.notes().length === 1, null, { timeout: 15000 });
    const note = await page.evaluate(() => window.__sdFeedback.notes()[0]);
    check('selector finds exactly that element', await page.evaluate((s) => document.querySelectorAll(s).length === 1, note.el.selector), note.el.selector);
    // The click may land on what is inside the button: the note is about the element under the pointer.
    const real = await page.evaluate((s) => Math.round(document.querySelector(s).getBoundingClientRect().width), note.el.selector);
    check('size and delta kept', note.el.rect.w === real && note.delta && note.delta.dw === 40, JSON.stringify([real, note.el.rect, note.delta]));
    check('a picture of the area', /^data:image\/jpeg;base64,/.test(note.shot) && note.shot.length > 2000, note.shot.slice(0, 40));
    check('the speaker log, secrets cut', /marker line for the feedback log/.test(note.log) && !note.log.includes(SECRET), note.log.slice(-200));
    check('the errors before the note', note.errors.some((x) => /e2e boom/.test(x.text)));

    console.log('4. «работает» and a bug of the whole page');
    await page.keyboard.press('Alt+KeyF');
    const r2 = await page.evaluate(() => { const h = [...document.querySelectorAll('h1,h2,h3')].find((x) => { const b = x.getBoundingClientRect(); return b.width > 0 && b.top > 0 && b.bottom < innerHeight; }); const b = h.getBoundingClientRect(); return { x: b.left + 5, y: b.top + b.height / 2 }; });
    await page.mouse.move(r2.x, r2.y);
    await page.mouse.click(r2.x, r2.y);
    await page.locator('[data-sd] >> css=.kinds button[data-k="works"]').click();
    await page.locator('[data-sd] >> css=.card .ok').click();
    await page.waitForFunction(() => window.__sdFeedback.notes().length === 2, null, { timeout: 15000 });
    page.once('dialog', (dl) => dl.accept('плеер не обновляется'));
    await page.locator('[data-sd] >> css=.fab').click();
    await page.locator('[data-sd] >> css=.pagebug').click();
    await page.waitForFunction(() => window.__sdFeedback.notes().length === 3, null, { timeout: 15000 });

    console.log('5. the prompt');
    const p = await page.evaluate(() => window.__sdFeedback.buildPrompt());
    const t = p.text;
    for (const [what, re] of [['context', /## Контекст проекта[\s\S]*src\/web\.html/], ['the change', /### 1\. Увеличить[\s\S]*ширина \+40 px/],
      ['the comment', /Комментарий: кнопка мелкая/], ['the selector', new RegExp('`' + note.el.selector.replace(/[.*+?^${}()|[\]\\]/g, '\\$&') + '`')],
      ['bug with log', /## Не работает \(1\)[\s\S]*плеер не обновляется[\s\S]*marker line/], ['works', /## Работает — не ломать/],
      ['criteria', /## Критерии готовности/], ['pictures named', /shot-1\.jpg/]]) {
      check('prompt: ' + what, re.test(t));
    }
    check('prompt: no secret', !t.includes(SECRET));
    check('two pictures to attach', p.shots.length >= 2, p.shots.length);

    console.log('6. saved as a file, kept after a reload');
    const [dl] = await Promise.all([page.waitForEvent('download'), page.locator('[data-sd] >> css=.file').click()]);
    const zipPath = path.join(os.tmpdir(), 'sd-e2e.zip');
    await dl.saveAs(zipPath);
    let names = '';
    try { names = execFileSync('python3', ['-I', '-c', 'import sys,zipfile;z=zipfile.ZipFile(sys.argv[1]);assert z.testzip() is None;print(" ".join(z.namelist()));print(z.read("prompt.md").decode()[:40])', zipPath]).toString(); } catch (x) { names = String(x); }
    check('zip: prompt.md and pictures, CRC ok', /prompt\.md shot-1\.jpg shot-2\.jpg/.test(names) && /# Задача/.test(names), names);
    await page.reload();
    await page.waitForFunction(() => window.__sdFeedback && window.__sdFeedback.notes().length === 3, null, { timeout: 5000 }).catch(() => {});
    check('notes survive a reload', (await page.evaluate(() => window.__sdFeedback.notes().length)) === 3);
    await page.screenshot({ path: path.join(os.tmpdir(), 'sd-e2e-panel.png') });
    const card2 = await page.evaluate(() => getComputedStyle(document.querySelector('body > div[data-sd]')).zIndex);
    check('the overlay is on top', card2 === '2147483647');
  } finally {
    d.kill();
  }

  if (USER_BIN) {
    console.log('7. the normal build has none of it');
    const PORT2 = 18878;
    const u = await start(USER_BIN, PORT2);
    try {
      const page = await browser.newPage();
      await page.goto(`http://127.0.0.1:${PORT2}/`);
      check('no feedback on the page', (await page.evaluate(() => !window.SOUNDYA_DEV && !document.querySelector('[data-sd]') && !window.__sdErrors)));
      const js = await page.request.get(`http://127.0.0.1:${PORT2}/dev/feedback.js`);
      check('no /dev/feedback.js', js.status() === 404, js.status());
      const log = await page.request.post(`http://127.0.0.1:${PORT2}/api/dev/log`, { data: '10', headers: { Origin: `http://127.0.0.1:${PORT2}` } });
      check('no /api/dev/log', log.status() === 404, log.status());
    } finally {
      u.d.kill();
    }
  }
  await browser.close();
  console.log(fails ? `FAILED: ${fails}` : 'all ok');
  process.exit(fails ? 1 : 0);
})().catch((e) => { console.error(e); process.exit(1); });
