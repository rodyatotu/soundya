// soundya developer build: feedback mode. Only in builds with the `dev` feature
// (src/devpage.rs puts it into the page); the normal build has none of it.
//
// Point at any element → it is outlined; click → write what to do with it
// («уменьшить», «не работает»…), drag the frame to show by how much, mark
// «работает» / «не работает». Every note keeps the CSS selector, size, text,
// styles, a picture of the area, the page's last errors and the speaker's log.
// «Собрать промпт» joins it all into one prompt for any AI (Claude, ChatGPT…).
// Alt+F — pick an element; in pick mode ↑/↓ — parent/child, Enter — take, Esc — out.
(function () {
  'use strict';
  var cfg = window.SOUNDYA_DEV || {};
  var KEY = 'soundya-dev-notes:' + (cfg.app || 'page');
  var KINDS = {
    smaller: { label: 'Уменьшить', icon: '－' },
    bigger: { label: 'Увеличить', icon: '＋' },
    move: { label: 'Сдвинуть', icon: '↔' },
    broken: { label: 'Не работает', icon: '✖' },
    works: { label: 'Работает', icon: '✔' },
    remove: { label: 'Убрать', icon: '🗑' },
    other: { label: 'Другое', icon: '✎' },
  };
  var CONTEXT = {
    panel:
      'soundya — программа на Rust для рутованных Яндекс Станций (Лайт, Мини): играет SoundCloud по голосу Алисы, ' +
      'AirPlay, управляет ПК и ТВ, обход блокировок, подсветка. Веб-панель колонки — один файл `src/web.html` ' +
      '(HTML+CSS+JS без сборщика и фреймворков, стиль «стекло» в духе Apple Liquid Glass), её отдаёт колонка на :8765 ' +
      '(`src/web.rs`); данные — `GET /api/status` (раз в секунду), команды — `POST /api/cmd` (текст), настройки — ' +
      '`POST /api/settings` (JSON merge-патч). Обработчики команд — `src/main.rs`.',
    wizard:
      'soundya — программа на Rust для рутованных Яндекс Станций. Мастер установки — `installer/src/wizard.html` ' +
      '(HTML+CSS+JS без сборщика), его отдаёт локальный сервер установщика `installer/src/main.rs` ' +
      '(на Mac — в своём окне soundya.app через WKWebView, `installer/src/macwin.rs`; на Windows — в браузере). ' +
      'Шаги: hello → find → features → install → music → pc → done; API `/api/info|scan|probe|install|job|settings|pc`.',
  };
  var RULES =
    '- Делай минимальные правки только в указанных местах; не трогай то, что отмечено «Работает».\n' +
    '- Не добавляй зависимостей и сборщиков; не меняй HTTP API без необходимости.\n' +
    '- Размеры из пометок — целевые (±2 px); где «на сколько» не указано — выбери сам и напиши, что выбрал.\n' +
    '- Для багов: сначала причина в коде (по ошибкам и журналу ниже), потом исправление.\n' +
    '- Проверь в узком окне (телефон, 375 px) и в широком; светлая/тёмная тема, если есть.\n' +
    '- Ответ: причина для каждого бага, список изменённых файлов с diff, что проверено и что нет. Отвечай по-русски.';

  // ------------------------------------------------------------ storage
  var notes = [];
  var goal = '';
  function local() {
    try { return JSON.parse(localStorage.getItem(KEY) || '{}'); } catch (e) { return {}; }
  }
  function load() {
    if (cfg.store) {
      return fetch(cfg.store).then(function (r) { return r.ok ? r.json() : {}; }).then(function (d) {
        // Notes kept only in this browser before (older builds): moved to the store once.
        // The browser's copy stays as it is, as a backup.
        var mine = local();
        if (!(d && d.notes && d.notes.length) && mine.notes && mine.notes.length) {
          apply(mine);
          save();
          toast('пометки из этого браузера перенесены на колонку: ' + mine.notes.length);
          return;
        }
        apply(d);
      }, function () { apply(local()); });
    }
    try { apply(JSON.parse(localStorage.getItem(KEY) || '{}')); } catch (e) {}
    return Promise.resolve();
  }
  function apply(d) {
    notes = (d && d.notes) || [];
    goal = (d && d.goal) || '';
    render();
  }
  function save() {
    var data = { notes: notes, goal: goal };
    if (cfg.store) {
      return fetch(cfg.store, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(data) })
        .then(function (r) { if (!r.ok) toast('не сохранилось: ' + r.status); }, function (e) { toast('не сохранилось: ' + e); });
    }
    try {
      localStorage.setItem(KEY, JSON.stringify(data));
    } catch (e) {
      // localStorage is ~5 MB: the oldest pictures go first.
      for (var i = 0; i < notes.length; i++) {
        if (notes[i].shot) {
          notes[i].shot = '';
          notes[i].shotLost = true;
          try { localStorage.setItem(KEY, JSON.stringify(data)); toast('место кончилось: старые скриншоты удалены'); return; } catch (x) {}
        }
      }
      toast('не сохранилось: ' + e.message);
    }
  }

  // ------------------------------------------------------------ the UI (shadow DOM: the page's styles do not reach it)
  var host = document.createElement('div');
  host.setAttribute('data-sd', '');
  host.style.cssText = 'position:fixed;inset:0;pointer-events:none;z-index:2147483647';
  var root = host.attachShadow({ mode: 'open' });
  root.innerHTML =
    '<style>' +
    ':host{all:initial}*{box-sizing:border-box;font:13px/1.35 -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,sans-serif}' +
    '.fab{position:fixed;left:12px;bottom:12px;pointer-events:auto;background:#ff2e6e;color:#fff;border:0;border-radius:18px;padding:7px 12px;font-weight:800;cursor:pointer;box-shadow:0 4px 16px #0006}' +
    '.fab b{background:#fff;color:#ff2e6e;border-radius:9px;padding:0 6px;margin-left:6px}' +
    '.box{position:fixed;border:2px solid #ff7a1a;background:#ff7a1a22;pointer-events:none;display:none}' +
    '.tag{position:fixed;background:#111d;color:#fff;padding:2px 6px;border-radius:4px;font:11px ui-monospace,Menlo,monospace;pointer-events:none;display:none;max-width:70vw;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}' +
    '.ghost{position:fixed;border:2px dashed #2e9bff;background:#2e9bff18;pointer-events:auto;cursor:move;display:none}' +
    '.h{position:absolute;width:12px;height:12px;background:#2e9bff;border:2px solid #fff;border-radius:3px}' +
    '.card,.drawer{position:fixed;pointer-events:auto;background:#1b1622f2;color:#f4eefc;border:1px solid #fff3;border-radius:14px;box-shadow:0 10px 40px #000a;padding:12px}' +
    '.card{width:320px;display:none}' +
    '.drawer{right:12px;bottom:56px;width:min(420px,calc(100vw - 24px));max-height:calc(100vh - 80px);overflow:auto;display:none}' +
    'h3{margin:0 0 8px;font-size:14px;font-weight:800}' +
    '.chips{display:flex;flex-wrap:wrap;gap:5px;margin:6px 0}' +
    'button{cursor:pointer;border:1px solid #fff3;background:#ffffff14;color:inherit;border-radius:9px;padding:5px 9px}' +
    'button:hover{background:#ffffff26}button.on{background:#ff7a1a;border-color:#ff7a1a;color:#fff}' +
    'button.main{background:linear-gradient(135deg,#ff7a1a,#ff2e6e);border:0;color:#fff;font-weight:700}' +
    'textarea{width:100%;min-height:58px;resize:vertical;background:#0006;color:inherit;border:1px solid #fff3;border-radius:9px;padding:6px}' +
    '.row{display:flex;gap:6px;flex-wrap:wrap;margin-top:8px}.muted{opacity:.65;font-size:12px}' +
    '.delta{font:12px ui-monospace,Menlo,monospace;color:#8fd0ff;margin:4px 0}' +
    '.note{border-top:1px solid #fff2;padding:7px 0;display:flex;gap:8px;align-items:flex-start}' +
    '.note img{width:64px;height:40px;object-fit:cover;border-radius:5px;flex:none}' +
    '.note .t{flex:1;min-width:0;word-break:break-word}.note code{font:11px ui-monospace,Menlo,monospace;opacity:.7}' +
    '.toast{position:fixed;left:50%;bottom:60px;transform:translateX(-50%);background:#111e;color:#fff;padding:8px 14px;border-radius:10px;display:none;pointer-events:none}' +
    '</style>' +
    '<div class="box"></div><div class="tag"></div>' +
    '<div class="ghost"><i class="h" data-h="nw"></i><i class="h" data-h="n"></i><i class="h" data-h="ne"></i><i class="h" data-h="e"></i>' +
    '<i class="h" data-h="se"></i><i class="h" data-h="s"></i><i class="h" data-h="sw"></i><i class="h" data-h="w"></i></div>' +
    '<div class="card"><h3>Что сделать с элементом</h3><div class="muted sel"></div><div class="chips kinds"></div>' +
    '<div class="delta"></div><textarea class="text" placeholder="Комментарий: что не так, как должно быть"></textarea>' +
    '<div class="muted">Потяните синюю рамку, чтобы показать нужный размер или место.</div>' +
    '<div class="row"><button class="main ok">Сохранить пометку</button><button class="reset">Сбросить рамку</button><button class="cancel">Отмена</button></div></div>' +
    '<div class="drawer"><h3>Фидбек · ' + esc(cfg.title || cfg.app || '') + ' · ' + esc(cfg.version || '') + '</h3>' +
    '<textarea class="goal" placeholder="Общая задача (необязательно): что хотите получить в итоге"></textarea>' +
    '<div class="row"><button class="main pick">Выбрать элемент (Alt+F)</button><button class="pagebug">Баг страницы целиком</button></div>' +
    '<div class="row"><button class="build">Собрать промпт</button><button class="copy">Копировать</button><button class="file">Сохранить файлом</button><button class="clear">Очистить</button></div>' +
    '<div class="list"></div><div class="muted errs"></div></div>' +
    '<button class="fab">DEV<b>0</b></button><div class="toast"></div>';
  var $ = function (s) { return root.querySelector(s); };
  var box = $('.box'), tag = $('.tag'), ghost = $('.ghost'), card = $('.card'), drawer = $('.drawer');

  function esc(s) {
    return String(s).replace(/[&<>"]/g, function (c) { return { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]; });
  }
  var toastTimer;
  function toast(t) {
    var el = $('.toast');
    el.textContent = t;
    el.style.display = 'block';
    clearTimeout(toastTimer);
    toastTimer = setTimeout(function () { el.style.display = 'none'; }, 2600);
  }

  // ------------------------------------------------------------ describing an element
  function selector(el) {
    if (!(el instanceof Element)) return '';
    var parts = [];
    for (var e = el; e && e.nodeType === 1 && e !== document.documentElement; e = e.parentElement) {
      if (e.id && document.querySelectorAll('#' + CSS.escape(e.id)).length === 1) {
        parts.unshift('#' + CSS.escape(e.id));
        break;
      }
      var p = e.tagName.toLowerCase();
      var cls = Array.prototype.filter.call(e.classList, function (c) { return !/^(on|active|open|show|hover)$/.test(c); }).slice(0, 2);
      if (cls.length) p += '.' + cls.map(CSS.escape).join('.');
      var par = e.parentElement;
      if (par) {
        var same = Array.prototype.filter.call(par.children, function (c) { return c.tagName === e.tagName; });
        if (same.length > 1) p += ':nth-of-type(' + (same.indexOf(e) + 1) + ')';
      }
      parts.unshift(p);
      try { if (document.querySelectorAll(parts.join(' > ')).length === 1) break; } catch (x) {}
    }
    return parts.join(' > ');
  }
  function where(el) {
    // The nearest section with an id or a heading: «где на странице».
    var out = [];
    for (var e = el.parentElement; e && e !== document.body; e = e.parentElement) {
      if (e.id) out.unshift('#' + e.id);
      if (out.length >= 3) break;
    }
    var sec = el.closest('section,[role=tabpanel],.card,.panel,.step,dialog,form');
    var head = sec && sec.querySelector('h1,h2,h3,h4,legend,.title');
    return { path: out.join(' › '), heading: head ? head.textContent.trim().slice(0, 80) : '' };
  }
  function styles(el) {
    var s = getComputedStyle(el);
    var keep = ['display', 'position', 'width', 'height', 'margin', 'padding', 'font-size', 'font-weight', 'line-height',
      'color', 'background-color', 'border-radius', 'gap', 'opacity'];
    var o = {};
    keep.forEach(function (k) { var v = s.getPropertyValue(k); if (v && v !== 'normal' && v !== 'none' && v !== '0px') o[k] = v; });
    return o;
  }
  function describe(el) {
    var r = el.getBoundingClientRect();
    var w = where(el);
    var text = (el.innerText || el.value || el.getAttribute('aria-label') || el.title || '').replace(/\s+/g, ' ').trim();
    return {
      selector: selector(el),
      tag: el.tagName.toLowerCase(),
      id: el.id || '',
      classes: Array.prototype.join.call(el.classList, ' '),
      text: text.slice(0, 160),
      rect: { x: Math.round(r.left + scrollX), y: Math.round(r.top + scrollY), w: Math.round(r.width), h: Math.round(r.height) },
      styles: styles(el),
      path: w.path,
      heading: w.heading,
      html: el.outerHTML.replace(/\s+/g, ' ').slice(0, 300),
    };
  }

  // ------------------------------------------------------------ picking
  var picking = false, hover = null, picked = null, kind = 'other', orig = null, cur = null;
  function ours(el) { return el === host || (el && el.closest && el.closest('[data-sd]')); }
  function outline(el) {
    if (!el) { box.style.display = tag.style.display = 'none'; return; }
    var r = el.getBoundingClientRect();
    Object.assign(box.style, { display: 'block', left: r.left + 'px', top: r.top + 'px', width: r.width + 'px', height: r.height + 'px' });
    tag.textContent = selector(el) + '  ' + Math.round(r.width) + '×' + Math.round(r.height);
    Object.assign(tag.style, { display: 'block', left: Math.max(4, r.left) + 'px', top: (r.top > 22 ? r.top - 20 : r.bottom + 4) + 'px' });
  }
  function startPick() {
    closeCard();
    picking = true;
    drawer.style.display = 'none';
    toast('Наведите на элемент и нажмите. ↑/↓ — родитель/вложенный, Esc — выход');
  }
  function stopPick() { picking = false; hover = null; outline(null); }
  document.addEventListener('mousemove', function (e) {
    if (!picking) return;
    var el = document.elementFromPoint(e.clientX, e.clientY);
    if (!el || ours(el)) return;
    hover = el;
    outline(el);
  }, true);
  ['click', 'mousedown', 'mouseup', 'pointerdown', 'pointerup'].forEach(function (ev) {
    document.addEventListener(ev, function (e) {
      if (!picking || ours(e.target)) return;
      e.preventDefault();
      e.stopPropagation();
      if (ev === 'click' && hover) { var el = hover; stopPick(); openCard(el); }
    }, true);
  });
  document.addEventListener('keydown', function (e) {
    if (e.altKey && (e.code === 'KeyF')) { e.preventDefault(); picking ? stopPick() : startPick(); return; }
    if (!picking) { if (e.key === 'Escape' && picked) closeCard(); return; }
    if (e.key === 'Escape') { stopPick(); e.preventDefault(); }
    else if (e.key === 'ArrowUp' && hover && hover.parentElement && hover.parentElement !== document.documentElement) { hover = hover.parentElement; outline(hover); e.preventDefault(); }
    else if (e.key === 'ArrowDown' && hover && hover.firstElementChild) { hover = hover.firstElementChild; outline(hover); e.preventDefault(); }
    else if (e.key === 'Enter' && hover) { var el = hover; stopPick(); openCard(el); e.preventDefault(); }
  }, true);

  // ------------------------------------------------------------ the note card and the frame
  var kinds = $('.kinds');
  Object.keys(KINDS).forEach(function (k) {
    var b = document.createElement('button');
    b.textContent = KINDS[k].icon + ' ' + KINDS[k].label;
    b.dataset.k = k;
    b.onclick = function () { kind = k; paintKinds(); };
    kinds.appendChild(b);
  });
  function paintKinds() {
    Array.prototype.forEach.call(kinds.children, function (b) { b.classList.toggle('on', b.dataset.k === kind); });
  }
  function openCard(el) {
    picked = el;
    kind = 'other';
    paintKinds();
    $('.text').value = '';
    var r = el.getBoundingClientRect();
    orig = { x: r.left, y: r.top, w: r.width, h: r.height };
    cur = Object.assign({}, orig);
    $('.sel').textContent = selector(el);
    placeGhost();
    outline(el);
    tag.style.display = 'none';
    card.style.display = 'block';
    var cw = 320, ch = card.offsetHeight || 260;
    var left = r.right + 12 + cw < innerWidth ? r.right + 12 : Math.max(8, r.left - cw - 12);
    if (left < 8 || left + cw > innerWidth) left = Math.max(8, (innerWidth - cw) / 2);
    card.style.left = left + 'px';
    card.style.top = Math.max(8, Math.min(innerHeight - ch - 8, r.top)) + 'px';
    showDelta();
    $('.text').focus();
  }
  function closeCard() {
    picked = null;
    card.style.display = ghost.style.display = 'none';
    outline(null);
  }
  function placeGhost() {
    Object.assign(ghost.style, { display: 'block', left: cur.x + 'px', top: cur.y + 'px', width: cur.w + 'px', height: cur.h + 'px' });
    var pos = { nw: [0, 0], n: [.5, 0], ne: [1, 0], e: [1, .5], se: [1, 1], s: [.5, 1], sw: [0, 1], w: [0, .5] };
    Array.prototype.forEach.call(ghost.children, function (h) {
      var p = pos[h.dataset.h];
      h.style.left = (p[0] * cur.w - 7) + 'px';
      h.style.top = (p[1] * cur.h - 7) + 'px';
      h.style.cursor = h.dataset.h + '-resize';
    });
  }
  function delta() {
    if (!orig) return null;
    var d = { dw: Math.round(cur.w - orig.w), dh: Math.round(cur.h - orig.h), dx: Math.round(cur.x - orig.x), dy: Math.round(cur.y - orig.y) };
    d.pw = orig.w ? Math.round(d.dw / orig.w * 100) : 0;
    d.ph = orig.h ? Math.round(d.dh / orig.h * 100) : 0;
    d.w = Math.round(cur.w); d.h = Math.round(cur.h);
    return d;
  }
  function deltaText(d) {
    if (!d) return '';
    var out = [];
    if (d.dw) out.push('ширина ' + sign(d.dw) + ' px (' + sign(d.pw) + ' %) → ' + d.w + ' px');
    if (d.dh) out.push('высота ' + sign(d.dh) + ' px (' + sign(d.ph) + ' %) → ' + d.h + ' px');
    if (d.dx) out.push('по горизонтали ' + sign(d.dx) + ' px');
    if (d.dy) out.push('по вертикали ' + sign(d.dy) + ' px');
    return out.join('; ');
  }
  function sign(n) { return (n > 0 ? '+' : '') + n; }
  function showDelta() { $('.delta').textContent = deltaText(delta()) || 'рамка не менялась'; }
  var drag = null;
  ghost.addEventListener('pointerdown', function (e) {
    drag = { h: e.target.dataset.h || 'move', x: e.clientX, y: e.clientY, start: Object.assign({}, cur) };
    ghost.setPointerCapture(e.pointerId);
    e.preventDefault();
  });
  ghost.addEventListener('pointermove', function (e) {
    if (!drag) return;
    var dx = e.clientX - drag.x, dy = e.clientY - drag.y, s = drag.start, h = drag.h;
    var n = Object.assign({}, s);
    if (h === 'move') { n.x = s.x + dx; n.y = s.y + dy; }
    if (h.indexOf('e') >= 0) n.w = Math.max(4, s.w + dx);
    if (h.indexOf('s') >= 0) n.h = Math.max(4, s.h + dy);
    if (h.indexOf('w') >= 0) { n.w = Math.max(4, s.w - dx); n.x = s.x + s.w - n.w; }
    if (h.indexOf('n') >= 0 && h !== 'move') { n.h = Math.max(4, s.h - dy); n.y = s.y + s.h - n.h; }
    cur = n;
    placeGhost();
    showDelta();
    // The kind follows the frame unless chosen by hand.
    var d = delta();
    if (kind === 'other' || kind === 'smaller' || kind === 'bigger' || kind === 'move') {
      kind = (d.dw || d.dh) ? ((d.dw + d.dh) < 0 ? 'smaller' : 'bigger') : ((d.dx || d.dy) ? 'move' : kind);
      paintKinds();
    }
  });
  ghost.addEventListener('pointerup', function () { drag = null; });
  $('.reset').onclick = function () { cur = Object.assign({}, orig); placeGhost(); showDelta(); };
  $('.cancel').onclick = closeCard;
  $('.ok').onclick = function () {
    var el = picked;
    if (!el) return;
    var d = delta();
    var changed = d && (d.dw || d.dh || d.dx || d.dy);
    var note = {
      id: Date.now(),
      time: new Date().toISOString(),
      kind: kind,
      comment: $('.text').value.trim(),
      el: describe(el),
      delta: changed ? d : null,
      page: pageInfo(),
      errors: (window.__sdErrors || []).slice(-12),
      shot: '',
      log: '',
    };
    var area = { x: orig.x, y: orig.y, w: orig.w, h: orig.h };
    closeCard();
    Promise.all([shoot(area, changed ? cur : null), journal()]).then(function (r) {
      note.shot = r[0];
      note.log = r[1];
      notes.push(note);
      save();
      render();
      toast('Пометка ' + notes.length + ' сохранена');
    });
  };
  $('.pagebug').onclick = function () {
    var text = prompt('Что не работает на странице?');
    if (!text) return;
    var note = { id: Date.now(), time: new Date().toISOString(), kind: 'broken', comment: text, el: null, delta: null,
      page: pageInfo(), errors: (window.__sdErrors || []).slice(-20), shot: '', log: '' };
    drawer.style.display = 'none';
    Promise.all([shoot({ x: 0, y: 0, w: innerWidth, h: innerHeight }, null), journal()]).then(function (r) {
      note.shot = r[0];
      note.log = r[1];
      notes.push(note);
      save();
      render();
      drawer.style.display = 'block';
    });
  };
  function pageInfo() {
    var active = document.querySelector('[aria-selected=true],.tab.on,.tab.active,nav .on,nav .active');
    return {
      url: location.pathname + location.search + location.hash,
      title: document.title,
      tab: active ? active.textContent.trim().slice(0, 40) : '',
      viewport: innerWidth + '×' + innerHeight,
      dpr: devicePixelRatio,
      scheme: matchMedia('(prefers-color-scheme: dark)').matches ? 'тёмная' : 'светлая',
      ua: navigator.userAgent,
    };
  }

  // ------------------------------------------------------------ picture of the area (html2canvas, loaded on first use)
  var h2c = null;
  function loadH2c() {
    if (window.html2canvas) return Promise.resolve(window.html2canvas);
    if (h2c) return h2c;
    h2c = new Promise(function (ok, no) {
      var s = document.createElement('script');
      s.src = cfg.h2c || '/dev/html2canvas.js';
      s.setAttribute('data-sd', '');
      s.onload = function () { ok(window.html2canvas); };
      s.onerror = function () { h2c = null; no(new Error('html2canvas не загрузился')); };
      document.head.appendChild(s);
    });
    return h2c;
  }
  // area and target are viewport rects; the picture is the area with 24 px around,
  // the original outlined orange and the wanted frame dashed blue.
  function shoot(area, target) {
    var pad = 24;
    var x = Math.max(0, Math.min(area.x, target ? target.x : area.x) - pad);
    var y = Math.max(0, Math.min(area.y, target ? target.y : area.y) - pad);
    var r = Math.min(innerWidth, Math.max(area.x + area.w, target ? target.x + target.w : 0) + pad);
    var b = Math.min(innerHeight, Math.max(area.y + area.h, target ? target.y + target.h : 0) + pad);
    var w = Math.max(8, r - x), h = Math.max(8, b - y);
    var scale = Math.min(1.5, 900 / w, devicePixelRatio || 1);
    return loadH2c().then(function (html2canvas) {
      return html2canvas(document.body, {
        x: x + scrollX, y: y + scrollY, width: w, height: h, scale: scale, logging: false, useCORS: true,
        backgroundColor: getComputedStyle(document.body).backgroundColor || '#000',
        ignoreElements: function (el) { return el.hasAttribute && el.hasAttribute('data-sd'); },
      });
    }).then(function (c) {
      var g = c.getContext('2d');
      g.lineWidth = 2;
      g.strokeStyle = '#ff7a1a';
      g.strokeRect((area.x - x) * scale, (area.y - y) * scale, area.w * scale, area.h * scale);
      if (target) {
        g.setLineDash([6, 4]);
        g.strokeStyle = '#2e9bff';
        g.strokeRect((target.x - x) * scale, (target.y - y) * scale, target.w * scale, target.h * scale);
      }
      return c.toDataURL('image/jpeg', 0.72);
    }).catch(function (e) {
      (window.__sdErrors || []).push({ t: '', kind: 'dev', text: 'скриншот: ' + e });
      return '';
    });
  }
  function journal() {
    if (!cfg.log) return Promise.resolve('');
    return fetch(cfg.log, { method: 'POST', body: '80' }).then(function (r) {
      return r.ok ? r.text() : '(журнал недоступен: ' + r.status + ')';
    }, function (e) { return '(журнал недоступен: ' + e + ')'; });
  }

  // ------------------------------------------------------------ the list
  function render() {
    $('.fab b').textContent = notes.length;
    $('.goal').value = goal;
    var list = $('.list');
    list.innerHTML = notes.length ? '' : '<p class="muted">Пометок нет. «Выбрать элемент» или Alt+F.</p>';
    notes.forEach(function (n, i) {
      var div = document.createElement('div');
      div.className = 'note';
      div.innerHTML = (n.shot ? '<img src="' + n.shot + '">' : '') +
        '<div class="t"><b>' + (i + 1) + '. ' + KINDS[n.kind].icon + ' ' + KINDS[n.kind].label + '</b> ' +
        esc(n.comment || '') + (n.delta ? '<div class="delta">' + esc(deltaText(n.delta)) + '</div>' : '') +
        '<br><code>' + esc(n.el ? n.el.selector : 'вся страница') + '</code></div><button title="Удалить">✕</button>';
      div.querySelector('button').onclick = function () { notes.splice(i, 1); save(); render(); };
      list.appendChild(div);
    });
    var errs = window.__sdErrors || [];
    $('.errs').textContent = errs.length ? 'Ошибок на странице с загрузки: ' + errs.length + ' (последняя: ' + errs[errs.length - 1].text.slice(0, 120) + ')' : '';
  }
  $('.goal').addEventListener('change', function () { goal = this.value; save(); });
  $('.fab').onclick = function () {
    var open = drawer.style.display === 'block';
    drawer.style.display = open ? 'none' : 'block';
    if (!open) render();
  };
  $('.pick').onclick = startPick;
  $('.clear').onclick = function () {
    if (!notes.length || !confirm('Удалить все пометки (' + notes.length + ')?')) return;
    notes = [];
    save();
    render();
  };

  // ------------------------------------------------------------ the prompt
  function buildPrompt() {
    var shots = [];
    var L = [];
    var p = pageInfo();
    L.push('# Задача: правки в ' + (cfg.title || cfg.app) + ' (soundya ' + (cfg.version || '') + ')');
    L.push('');
    L.push('## Контекст проекта');
    L.push(CONTEXT[cfg.app] || CONTEXT.panel);
    L.push('');
    L.push('## Окружение');
    L.push('- Сборка: ' + (cfg.version || '?') + ' (версия разработчика)' + (cfg.device ? ', устройство: ' + cfg.device : ''));
    L.push('- Окно: ' + p.viewport + ', плотность ' + p.dpr + ', тема системы ' + p.scheme);
    L.push('- Браузер: ' + p.ua);
    L.push('- Собрано: ' + new Date().toLocaleString('ru-RU'));
    if (goal.trim()) { L.push(''); L.push('## Общая задача'); L.push(goal.trim()); }
    var todo = notes.filter(function (n) { return n.kind !== 'works' && n.kind !== 'broken'; });
    var bugs = notes.filter(function (n) { return n.kind === 'broken'; });
    var fine = notes.filter(function (n) { return n.kind === 'works'; });
    function shotName(n) {
      if (!n.shot) return n.shotLost ? 'скриншот не сохранился (кончилось место)' : '';
      var name = 'shot-' + (shots.length + 1) + '.jpg';
      shots.push({ name: name, data: n.shot });
      return name + ' (оранжевая рамка — элемент сейчас' + (n.delta ? ', синяя пунктирная — каким он должен стать' : '') + ')';
    }
    function element(n) {
      var e = n.el;
      if (!e) return ['- Где: вся страница ' + n.page.url + (n.page.tab ? ', вкладка «' + n.page.tab + '»' : '')];
      var out = ['- Элемент: `' + e.selector + '`'];
      out.push('- Что это: <' + e.tag + '>' + (e.classes ? ' class="' + e.classes + '"' : '') + (e.text ? ', текст «' + e.text + '»' : ''));
      out.push('- Где: ' + [n.page.tab && 'вкладка «' + n.page.tab + '»', e.heading && 'раздел «' + e.heading + '»', e.path].filter(Boolean).join(', ') +
        ' (' + n.page.url + ', окно ' + n.page.viewport + ')');
      out.push('- Сейчас: ' + e.rect.w + '×' + e.rect.h + ' px в (' + e.rect.x + ', ' + e.rect.y + '); стили: ' +
        Object.keys(e.styles).map(function (k) { return k + ': ' + e.styles[k]; }).join('; '));
      out.push('- HTML: `' + e.html.replace(/`/g, "'") + '`');
      return out;
    }
    var k = 0;
    if (todo.length) {
      L.push(''); L.push('## Правки (' + todo.length + ')');
      todo.forEach(function (n) {
        k++;
        L.push(''); L.push('### ' + k + '. ' + KINDS[n.kind].label + (n.el ? ': ' + (n.el.text ? '«' + n.el.text.slice(0, 40) + '»' : n.el.tag) : ''));
        L.push('- Что сделать: ' + KINDS[n.kind].label.toLowerCase() + (n.delta ? ' — ' + deltaText(n.delta) : (n.kind === 'smaller' || n.kind === 'bigger' || n.kind === 'move' ? ' (на сколько — не указано, выбери сам)' : '')));
        if (n.comment) L.push('- Комментарий: ' + n.comment);
        L.push.apply(L, element(n));
        var s = shotName(n); if (s) L.push('- Скриншот: ' + s);
      });
    }
    if (bugs.length) {
      L.push(''); L.push('## Не работает (' + bugs.length + ')');
      bugs.forEach(function (n) {
        k++;
        L.push(''); L.push('### ' + k + '. ' + (n.comment || 'не работает'));
        L.push.apply(L, element(n));
        var s = shotName(n); if (s) L.push('- Скриншот: ' + s);
        L.push('- Время пометки: ' + n.time);
        if (n.errors && n.errors.length) {
          L.push('- Ошибки страницы перед пометкой:'); L.push('```');
          n.errors.forEach(function (e) { L.push(e.t + ' [' + e.kind + '] ' + e.text); }); L.push('```');
        } else L.push('- Ошибок JS и неудачных запросов не было.');
        if (n.log) { L.push('- Журнал (' + (cfg.app === 'wizard' ? 'установщик' : 'колонка, /tmp/soundya.log') + ', секреты вырезаны):'); L.push('```'); L.push(n.log.trim()); L.push('```'); }
      });
    }
    if (fine.length) {
      L.push(''); L.push('## Работает — не ломать');
      fine.forEach(function (n) { L.push('- ' + (n.el ? '`' + n.el.selector + '`' + (n.el.text ? ' «' + n.el.text.slice(0, 50) + '»' : '') : 'страница') + (n.comment ? ' — ' + n.comment : '')); });
    }
    L.push(''); L.push('## Правила');
    L.push(RULES);
    L.push(''); L.push('## Критерии готовности');
    if (todo.length) L.push('- Каждая правка из «Правки» сделана; размеры и сдвиги совпадают с указанными (±2 px) в том же окне ' + p.viewport + '.');
    if (bugs.length) L.push('- Каждый пункт «Не работает» исправлен: названа причина в коде, после исправления ошибок из пометки больше нет.');
    if (fine.length) L.push('- Всё из «Работает — не ломать» работает как раньше.');
    L.push('- Нет новых ошибок в консоли; `cargo test` проходит' + (cfg.app === 'wizard' ? ' (и `cd installer && cargo test`)' : '') + '.');
    if (shots.length) { L.push(''); L.push('Скриншоты приложены отдельными файлами: ' + shots.map(function (s) { return s.name; }).join(', ') + '.'); }
    return { text: L.join('\n'), shots: shots };
  }
  var built = null;
  $('.build').onclick = function () {
    if (!notes.length && !goal.trim()) { toast('Нет пометок'); return; }
    built = buildPrompt();
    copy(built.text);
  };
  $('.copy').onclick = function () { copy((built = buildPrompt()).text); };
  function copy(text) {
    var done = function () { toast('Промпт скопирован (' + Math.round(text.length / 1024) + ' КБ)' + (built && built.shots.length ? '; скриншоты — «Сохранить файлом»' : '')); };
    // The panel is plain http on the home network: navigator.clipboard is not there.
    if (navigator.clipboard && window.isSecureContext) {
      navigator.clipboard.writeText(text).then(done, function () { fallback(text) ? done() : toast('Не скопировалось'); });
    } else if (fallback(text)) done(); else toast('Не скопировалось — «Сохранить файлом»');
  }
  function fallback(text) {
    var ta = document.createElement('textarea');
    ta.value = text;
    ta.setAttribute('data-sd', '');
    ta.style.cssText = 'position:fixed;left:-9999px;top:0';
    document.body.appendChild(ta);
    ta.select();
    var ok = false;
    try { ok = document.execCommand('copy'); } catch (e) {}
    ta.remove();
    return ok;
  }
  $('.file').onclick = function () {
    built = buildPrompt();
    var stamp = new Date().toISOString().slice(0, 16).replace(/[:T]/g, '-');
    var files = [{ name: 'prompt.md', data: utf8(built.text) }].concat(built.shots.map(function (s) {
      return { name: s.name, data: b64bytes(s.data.split(',')[1]) };
    }));
    var name = 'soundya-feedback-' + (cfg.app || 'page') + '-' + stamp;
    if (cfg.save) {
      // The Mac app's window cannot download: the installer writes the files to ~/Downloads.
      fetch(cfg.save, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({
        name: name, files: files.map(function (f) { return { name: f.name, base64: bytesB64(f.data) }; }),
      }) }).then(function (r) { return r.text().then(function (t) { toast(r.ok ? 'Сохранено: ' + t : 'Не сохранилось: ' + t); }); },
        function (e) { toast('Не сохранилось: ' + e); });
      return;
    }
    var blob = files.length === 1 ? new Blob([files[0].data], { type: 'text/markdown' }) : zip(files);
    var a = document.createElement('a');
    a.href = URL.createObjectURL(blob);
    a.download = name + (files.length === 1 ? '.md' : '.zip');
    a.setAttribute('data-sd', '');
    document.body.appendChild(a);
    a.click();
    setTimeout(function () { URL.revokeObjectURL(a.href); a.remove(); }, 1000);
  };
  function utf8(s) { return new TextEncoder().encode(s); }
  function b64bytes(b) { var s = atob(b), u = new Uint8Array(s.length); for (var i = 0; i < s.length; i++) u[i] = s.charCodeAt(i); return u; }
  function bytesB64(u) { var s = ''; for (var i = 0; i < u.length; i += 0x8000) s += String.fromCharCode.apply(null, u.subarray(i, i + 0x8000)); return btoa(s); }
  // A plain (stored, not compressed) zip: prompt.md and the pictures.
  var crcTable = null;
  function crc32(u) {
    if (!crcTable) { crcTable = []; for (var n = 0; n < 256; n++) { var c = n; for (var k = 0; k < 8; k++) c = c & 1 ? 0xEDB88320 ^ (c >>> 1) : c >>> 1; crcTable[n] = c >>> 0; } }
    var crc = 0xFFFFFFFF;
    for (var i = 0; i < u.length; i++) crc = crcTable[(crc ^ u[i]) & 0xFF] ^ (crc >>> 8);
    return (crc ^ 0xFFFFFFFF) >>> 0;
  }
  function zip(files) {
    var parts = [], central = [], offset = 0;
    files.forEach(function (f) {
      var name = utf8(f.name), crc = crc32(f.data), size = f.data.length;
      var h = new DataView(new ArrayBuffer(30));
      h.setUint32(0, 0x04034b50, true); h.setUint16(4, 20, true); h.setUint16(6, 0x0800, true);
      h.setUint32(14, crc, true); h.setUint32(18, size, true); h.setUint32(22, size, true); h.setUint16(26, name.length, true);
      parts.push(h, name, f.data);
      var c = new DataView(new ArrayBuffer(46));
      c.setUint32(0, 0x02014b50, true); c.setUint16(4, 20, true); c.setUint16(6, 20, true); c.setUint16(8, 0x0800, true);
      c.setUint32(16, crc, true); c.setUint32(20, size, true); c.setUint32(24, size, true); c.setUint16(28, name.length, true);
      c.setUint32(42, offset, true);
      central.push(c, name);
      offset += 30 + name.length + size;
    });
    var csize = central.reduce(function (a, p) { return a + p.byteLength; }, 0);
    var e = new DataView(new ArrayBuffer(22));
    e.setUint32(0, 0x06054b50, true); e.setUint16(8, files.length, true); e.setUint16(10, files.length, true);
    e.setUint32(12, csize, true); e.setUint32(16, offset, true);
    return new Blob(parts.concat(central, [e]), { type: 'application/zip' });
  }
  // For tests (tests/dev_feedback_e2e.py).
  window.__sdFeedback = { buildPrompt: buildPrompt, notes: function () { return notes; }, zip: zip, selector: selector };

  function mount() {
    document.body.appendChild(host);
    load();
  }
  if (document.body) mount(); else document.addEventListener('DOMContentLoaded', mount);
})();
