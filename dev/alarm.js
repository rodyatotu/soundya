// soundya developer build: the alarm that cannot be slept through (src/alarm.rs), on the
// panel's home tab. Only the developer build serves this file.
(() => {
  if ((window.SOUNDYA_DEV || {}).app !== 'panel') return;
  const home = document.getElementById('tab-home');
  if (!home) return;
  const DAYS = { once: 'один раз', daily: 'каждый день', weekdays: 'по будням', weekend: 'по выходным' };
  const STAGE = { Music: 'играет музыка', Alice: 'говорит Алиса', Check: 'спрашивает «Вы проснулись?»', Question: 'ждёт ответ на пример', Snoozed: 'отложен на 5 минут' };
  const srcWord = s => !s ? 'ваша музыка' : s === 'likes' ? 'лайки' : s === 'recs' ? 'рекомендации'
    : s.startsWith('playlist:') ? 'плейлист «' + s.slice(9) + '»' : s.startsWith('play:') ? '«' + s.slice(5) + '»' : s;
  const send = (cmd) => fetch('/api/cmd', { method: 'POST', body: cmd }).then(r => r.text()).catch(e => 'error: ' + e);

  const box = document.createElement('div');
  box.className = 'widget';
  box.dataset.w = 'alarm-dev';
  // The panel's own look: its fields, buttons and hints, nothing of its own.
  const FIELD = 'font:inherit;color:var(--ink);background:var(--field);border:1px solid var(--glass-edge);border-radius:14px;padding:9px 11px;outline:none';
  box.innerHTML = `
    <div class="whead"><b>Будильник <small class="hint">dev</small></b></div>
    <div id="alring" hidden>
      <p><b>Звонит</b> <span class="hint" id="alstage"></span></p>
      <div class="row" style="gap:8px;flex-wrap:wrap"><button class="primary" id="alawake">Я проснулся</button><button id="alsnooze">Ещё 5 минут</button></div>
      <div id="alq" hidden style="margin-top:10px"><p id="alqtext"></p>
        <div class="row" style="gap:8px"><input type="text" id="alans" inputmode="numeric" autocomplete="off" placeholder="Ответ" style="${FIELD};width:110px"><button class="primary" id="alanswer">Ответить</button></div></div>
    </div>
    <ul class="list" id="allist"></ul>
    <p class="hint" id="alempty">Будильников нет. Голосом: «Алиса, разбуди меня в 7 под мои лайки».</p>
    <div class="row" style="flex-wrap:wrap;gap:8px;margin-top:8px">
      <input type="time" id="altime" value="07:00">
      <select id="aldays" style="${FIELD}">${Object.entries(DAYS).map(([k, v]) => `<option value="${k}">${v}</option>`).join('')}</select>
      <input type="text" id="alsrc" placeholder="Под что: лайки, рекомендации, плейлист утро" style="flex:1;min-width:180px">
      <button class="primary" id="aladd">Поставить</button>
    </div>
    <p class="hint">Выключается только словами «Алиса, я проснулся» и ответом на пример. «Стоп», пауза и ▶‖ его не выключают.</p>
    <div class="msg" id="almsg"></div>`;
  home.insertBefore(box, home.firstElementChild ? home.firstElementChild.nextSibling : null);
  const $ = id => box.querySelector('#' + id);
  const show = t => { const m = $('almsg'); m.textContent = t.replace(/^OK:\s*/, ''); m.className = 'msg ' + (t.startsWith('error') ? 'err' : 'ok'); };

  let lastQuestion = '';
  async function load() {
    let j; try { j = JSON.parse(await send('alarm json')); } catch (e) { return; }
    const r = j.ringing;
    $('alring').hidden = !r;
    if (r) {
      $('alstage').textContent = '· ' + (STAGE[r.stage] || r.stage) + ' · круг ' + r.round + (r.snoozes ? ' · отложен ' + r.snoozes + ' раз' : '');
      $('alq').hidden = r.stage !== 'Question';
      if (r.stage === 'Question' && r.question && !lastQuestion) $('alqtext').textContent = `Сколько будет ${r.question[0]} плюс ${r.question[1]}?`;
    } else lastQuestion = '';
    const list = $('allist'); list.innerHTML = '';
    const on = (j.alarms || []).filter(a => a.on);
    $('alempty').hidden = on.length > 0;
    for (const a of on) {
      const li = document.createElement('li');
      const t = document.createElement('span'); t.textContent = `${a.hour}:${String(a.minute).padStart(2, '0')} · ${DAYS[a.days] || a.days} · ${srcWord(a.source)}`;
      const del = document.createElement('button'); del.className = 'link'; del.textContent = '✕'; del.title = 'Удалить';
      del.onclick = async () => { show(await send('alarm del ' + a.id)); load(); };
      li.append(t, del); list.appendChild(li);
    }
  }
  $('aladd').onclick = async () => {
    const [h, m] = ($('altime').value || '07:00').split(':').map(Number);
    show(await send('alarm add ' + JSON.stringify({ hour: h, minute: m, days: $('aldays').value, source: $('alsrc').value.trim() })));
    load();
  };
  $('alawake').onclick = async () => {
    const t = await send('alarm awake');
    lastQuestion = t.replace(/^OK:\s*/, ''); $('alqtext').textContent = lastQuestion; $('alq').hidden = false; $('alans').focus(); load();
  };
  $('alans').onkeydown = e => { if (e.key === 'Enter') $('alanswer').click(); };
  $('alanswer').onclick = async () => {
    const t = await send('alarm answer ' + ($('alans').value || '').trim());
    $('alans').value = ''; show(t);
    lastQuestion = /Неверно/.test(t) ? t.replace(/^OK:\s*/, '') : '';
    if (lastQuestion) $('alqtext').textContent = lastQuestion;
    load();
  };
  $('alsnooze').onclick = async () => { show(await send('alarm snooze')); load(); };
  load();
  setInterval(load, 5000);
})();
