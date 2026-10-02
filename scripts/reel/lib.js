// Shared engine for every GymClaw film. Pages set window.DURATION and window.render(t);
// render(t) must be a pure function of t so frames can be rendered in any order and in parallel.

const $ = s => document.querySelector(s);
const $$ = s => document.querySelectorAll(s);
const clamp = (x, a = 0, b = 1) => Math.min(b, Math.max(a, x));
const seg = (t, a, b) => clamp((t - a) / (b - a));
const lerp = (a, b, p) => a + (b - a) * p;
const oExpo = x => x >= 1 ? 1 : 1 - Math.pow(2, -10 * x);
const oCubic = x => 1 - Math.pow(1 - x, 3);
const ioCubic = x => x < .5 ? 4 * x * x * x : 1 - Math.pow(-2 * x + 2, 3) / 2;
const iCubic = x => x * x * x;
const oBack = (x, s = 1.7) => x <= 0 ? 0 : 1 + (s + 1) * Math.pow(x - 1, 3) + s * Math.pow(x - 1, 2);
// Damped spring 0→1 with overshoot; x is seconds since start.
const spring = (x, k = 9, f = 14) => x <= 0 ? 0 : 1 - Math.exp(-k * x) * Math.cos(f * x);
const IMG = s => `../../gymclaw/assets/workout-guide/${s}/frame-1.png`;
const CLAW = (size, color = '#0f120e') => `<svg width="${size}" height="${size}" viewBox="0 0 44 44"><path d="M15 31 23 12M21 33 29 14M27 34 33 20" stroke="${color}" stroke-width="4.4" stroke-linecap="round"/></svg>`;
const LOGO = size => `<svg width="${size}" height="${size}" viewBox="0 0 44 44"><circle cx="22" cy="22" r="22" fill="#d7f580"/><path d="M15 31 23 12M21 33 29 14M27 34 33 20" stroke="#0f120e" stroke-width="4.4" stroke-linecap="round"/></svg>`;
// Inline keyboard markup: kb(['Label','buttonId'], ['Other']).
const kb = (...b) => `<div class="kb">${b.map(([l, id]) => `<div${id ? ` id="${id}"` : ''}>${l}</div>`).join('')}</div>`;
// Value of a step schedule [[t0, v0], [t1, v1], ...] at time t.
const step = (t, list) => list.reduce((v, [at, x]) => t >= at ? x : v, list[0][1]);

/**
 * Telegram phone with a scripted chat.
 * script.msgs:  {k:'chip'|'bot'|'user'|'photo'|'userphoto'|'typing', at, end?, html, img?, kb?}  (at<0 = already there)
 * script.taps:  {at, id, label?, tint?}  finger tap on a button id; optional relabel and tint afterwards
 * script.typed: {from, send, text}       text typed into the input bar, sent at `send`
 * script.clock: [[t,'07:15'], ...]       status bar time
 * script.locks: {in, out, date, time, notifs:[{at, html}]}  lock screen windows
 */
function Phone(mount, script) {
  mount.innerHTML = `<div id="phone"><div id="screen">
    <div id="island"></div>
    <div id="status"><span id="clock"></span><span class="icons">
      <svg width="20" height="13" viewBox="0 0 20 13"><rect x="0" y="9" width="3.5" height="4" rx="1" fill="#fff"/><rect x="5.5" y="6" width="3.5" height="7" rx="1" fill="#fff"/><rect x="11" y="3" width="3.5" height="10" rx="1" fill="#fff"/><rect x="16.5" y="0" width="3.5" height="13" rx="1" fill="#fff"/></svg>
      <svg width="27" height="13" viewBox="0 0 27 13"><rect x=".5" y=".5" width="23" height="12" rx="3.5" fill="none" stroke="#fff" opacity=".5"/><rect x="2.5" y="2.5" width="17" height="8" rx="2" fill="#fff"/><rect x="25" y="4.5" width="1.6" height="4" rx=".8" fill="#fff" opacity=".5"/></svg></span></div>
    <div id="head"><div class="back">‹</div><div class="avatar" id="avatar">${CLAW(44)}</div>
      <div><div class="name">GymClaw</div><div class="hs" id="hsub">bot</div></div></div>
    <div id="chat"></div>
    <div id="input"><div class="clip">⌀</div><div id="field"><span class="ph" id="ph">Message</span><span id="typed"></span><span id="caret"></span></div>
      <div id="send"><svg width="24" height="24" viewBox="0 0 24 24"><path d="M3 11.5 21 3l-6.5 18-3-7.5z" fill="#6ab3f3"/></svg></div></div>
    <div id="lock"><div class="date"></div><div class="clock"></div><div id="notifs"></div><div class="hint">Swipe up to open</div></div>
    <div id="homebar"></div><div id="glare"></div></div></div>`;
  if (!$('#finger')) document.body.firstElementChild.insertAdjacentHTML('beforeend', '<div id="finger"></div><div id="ripple"></div>');
  const msgs = script.msgs, taps = script.taps || [], typed = script.typed || [], locks = script.locks || [];
  const chat = $('#chat');
  for (const m of msgs) {
    const row = document.createElement('div');
    row.className = 'row ' + (m.k === 'user' || m.k === 'userphoto' ? 'user' : m.k === 'chip' ? 'chip' : 'bot');
    let inner;
    if (m.k === 'chip') inner = `<div class="chipt">${m.html}</div>`;
    else if (m.k === 'typing') inner = `<div class="b"><div class="dots"><i></i><i></i><i></i></div></div>`;
    else if (m.k === 'photo' || m.k === 'userphoto') inner = `<div class="b photo">${m.img ? `<img src="${IMG(m.img)}">` : m.media}${m.html ? `<div class="cap">${m.html}</div>` : ''}</div>${m.kb || ''}`;
    else inner = `<div class="b">${m.html}</div>${m.kb || ''}`;
    row.innerHTML = `<div class="mw">${inner}</div>`;
    chat.appendChild(row);
    m.el = row; m.mw = row.firstChild;
  }
  for (const t of taps) t.orig = document.getElementById(t.id)?.textContent;
  const kicks = msgs.filter(m => m.at > 0 && m.k !== 'typing').map(m => m.at);

  function measure() { for (const m of msgs) m.h = m.el.scrollHeight; }

  function frame(t) {
    let typing = false;
    for (const m of msgs) {
      let p;
      if (m.k === 'typing') {
        p = oCubic(seg(t, m.at, m.at + .15)) * (1 - seg(t, m.end - .02, m.end + .1));
        if (t >= m.at && t < m.end) typing = true;
        if (p > 0) m.el.querySelectorAll('i').forEach((d, i) => { const w = .5 + .5 * Math.sin(t * 14 - i * .9); d.style.opacity = .35 + .65 * w; d.style.transform = `translateY(${-3 * w}px)`; });
      } else p = m.at < 0 ? 1 : oCubic(seg(t, m.at, m.at + .24));
      m.el.style.height = (m.h * p) + 'px';
      m.el.style.display = p <= 0 ? 'none' : 'flex';
      if (p <= 0) continue;
      const pop = m.at < 0 ? 1 : oBack(seg(t, m.at, m.at + .38), 1.4);
      m.mw.style.opacity = m.k === 'typing' ? p : clamp(seg(t, m.at, m.at + .12) + (m.at < 0));
      m.mw.style.transformOrigin = m.el.classList.contains('user') ? '100% 100%' : m.k === 'chip' ? '50% 50%' : '0 100%';
      m.mw.style.transform = `scale(${lerp(.82, 1, pop)}) translateY(${(1 - pop) * 16}px)`;
    }
    $('#hsub').textContent = typing ? 'typing…' : 'bot'; $('#hsub').className = 'hs' + (typing ? ' typing' : '');
    $('#clock').textContent = step(t, script.clock);
    // Input bar: letters appear one by one; cleared on send.
    let shown = '', caret = false;
    for (const ty of typed) {
      if (t >= ty.from - .2 && t < ty.send) {
        caret = true;
        const n = Math.ceil(clamp((t - ty.from) / (ty.send - .08 - ty.from)) * ty.text.length);
        shown = ty.text.slice(0, n);
      }
    }
    $('#typed').textContent = shown; $('#ph').style.display = shown ? 'none' : 'inline';
    $('#caret').style.opacity = caret ? (Math.floor(t * 3) % 2 ? 0 : 1) : 0;
    $('#send').style.transform = `scale(${shown ? 1.1 : .9})`; $('#send').style.opacity = shown ? 1 : .45;
    for (const tap of taps) {
      const b = document.getElementById(tap.id); if (!b) continue;
      const done = t >= tap.at + .12, pressed = t > tap.at - .05 && !done;
      b.textContent = done && tap.label ? tap.label : tap.orig;
      b.style.background = pressed ? (tap.tint === 'hot' ? 'rgba(255,106,69,.55)' : 'rgba(80,110,140,1)')
        : done ? (tap.tint === 'hot' ? 'rgba(255,106,69,.25)' : 'rgba(215,245,128,.22)') : '';
    }
    lockFrame(t); fingerFrame(t);
  }

  function lockFrame(t) {
    const L = $('#lock'), w = locks.find(l => t >= l.in - .05 && t < l.out + .5);
    if (!w) { L.style.display = 'none'; return; }
    const lockIn = seg(t, w.in - .02, w.in + .18), unlock = ioCubic(seg(t, w.out, w.out + .45));
    L.style.display = unlock < 1 ? 'block' : 'none';
    L.style.opacity = lockIn; L.style.transform = `translateY(${-unlock * 105}%)`;
    L.querySelector('.date').textContent = w.date; L.querySelector('.clock').textContent = w.time;
    L.querySelector('.clock').style.transform = `scale(${lerp(1.06, 1, oCubic(lockIn))})`;
    const box = $('#notifs');
    if (box.dataset.w !== w.time) {
      box.dataset.w = w.time;
      box.innerHTML = w.notifs.map(n => `<div class="notif"><div class="avatar">${CLAW(40)}</div><div style="flex:1"><div class="t">GymClaw<span>now</span></div>${n.html}</div></div>`).join('');
    }
    // Newest notification sits at the anchor; older ones are pushed down.
    const els = [...box.children];
    let y = 0;
    for (let i = els.length - 1; i >= 0; i--) {
      const n = w.notifs[i], el = els[i], p = oBack(seg(t, n.at, n.at + .45), 1.3);
      el.style.opacity = seg(t, n.at, n.at + .15);
      el.style.transform = `translateY(${y + (1 - p) * 60}px) scale(${lerp(.9, 1, p)})`;
      if (t >= n.at) y += (el.offsetHeight + 10) * oCubic(seg(t, n.at, n.at + .35));
    }
  }

  function fingerFrame(t) {
    const f = $('#finger'), r = $('#ripple');
    let shown = false;
    for (const tap of taps) {
      const el = document.getElementById(tap.id);
      if (!el || t <= tap.at - .55 || t >= tap.at + .45) continue;
      const b = el.getBoundingClientRect(), cx = b.left + b.width / 2, cy = b.top + b.height / 2;
      const inP = oCubic(seg(t, tap.at - .55, tap.at - .05)), outP = iCubic(seg(t, tap.at + .15, tap.at + .45));
      const press = t > tap.at - .06 && t < tap.at + .1;
      f.style.left = (cx - 23 + (1 - inP) * 160 + outP * 120) + 'px'; f.style.top = (cy - 23 + (1 - inP) * 140 + outP * 90) + 'px';
      f.style.opacity = Math.min(inP * 1.4, 1 - outP); f.style.transform = `scale(${press ? .82 : 1})`;
      const rp = seg(t, tap.at, tap.at + .42), R = 20 + oCubic(rp) * 70;
      r.style.width = r.style.height = R * 2 + 'px';
      r.style.left = cx - R + 'px'; r.style.top = cy - R + 'px'; r.style.opacity = rp > 0 ? (1 - rp) : 0;
      shown = true;
    }
    if (!shown) { f.style.opacity = 0; r.style.opacity = 0; }
  }

  // Small scale bump when a message lands; add to the page's own phone pose.
  function kick(t) {
    let k = 0;
    for (const at of kicks) { const d = t - at; if (d > 0 && d < .5) k += Math.exp(-d * 10) * Math.sin(d * 30) * .012; }
    return k;
  }
  return { frame, kick, measure };
}

// Kicker + masked headline lines + sub + viz enter/exit for one chapter element.
function chapter(el, t, a, b, { hold = b } = {}) {
  const vis = t >= a - .05 && t < hold + .02;
  el.style.display = vis ? 'block' : 'none'; if (!vis) return false;
  const exit = seg(t, hold - .32, hold);
  const k = el.querySelector('.kick');
  if (k) { k.style.opacity = seg(t, a, a + .25) * (1 - exit); k.style.transform = `translateX(${(1 - oExpo(seg(t, a, a + .5))) * -40}px)`; }
  el.querySelectorAll('.mask span').forEach((s, i) => {
    const p = oExpo(seg(t, a + .04 + i * .07, a + .62 + i * .07)), q = iCubic(seg(t, hold - .32 + i * .04, hold - .04 + i * .04));
    s.style.transform = `translateY(${(1 - p) * 112 - q * 112}%) skewY(${(1 - p) * 6}deg)`;
  });
  const sub = el.querySelector('.sub');
  if (sub) { sub.style.opacity = seg(t, a + .25, a + .6) * (1 - exit); sub.style.transform = `translateY(${(1 - oCubic(seg(t, a + .25, a + .7))) * 24}px)`; }
  const viz = el.querySelector('.viz');
  if (viz) { viz.style.opacity = 1 - exit; viz.style.transform = `translateY(${exit * -30}px)`; }
  return true;
}

// Waits for fonts and images, measures chat rows, then exposes render().
async function boot(phone, render, duration) {
  await document.fonts.ready;
  await Promise.all([...document.images].map(i => i.complete ? 0 : new Promise(r => { i.onload = i.onerror = r; })));
  phone?.measure();
  window.DURATION = duration;
  window.render = render;
  render(0);
  return true;
}
