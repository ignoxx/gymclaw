/* View of gymclaw state. Presentation only, except the Memory tab (the agent's notes and extensions) and the
   Exercises tab (plan targets), which edit through gymclaw's own tools. Made first for Telegram: the bot's
   menu button opens it as a Mini App (see "Telegram" below); a browser gets the same page. */
const SVG = 'http://www.w3.org/2000/svg';
const WEEKDAYS = ['Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat', 'Sun'];
let restTimer = null;
let state = null;
let selectedExercise = null;
const now = () => Date.now();
// CSS class from a data value, always prefixed: ('crowd', 'A few') → 'crowd-a-few'. A bare value can
// collide with a layout class (crowd "Empty" picked up the `.empty` placeholder margin).
const dataClass = (kind, value) => `${kind}-${String(value || '').toLowerCase().replace(/[^a-z0-9]+/g, '-')}`;

/* ---------- Telegram ----------
   `tg` is the Mini App API when Telegram opened the page, else null (a browser, or telegram.org didn't
   load), and every Telegram call is skipped. Features are gated on the client's Bot API version. */
const tg = window.Telegram?.WebApp?.initData ? window.Telegram.WebApp : null;
const tgHas = version => Boolean(tg?.isVersionAtLeast(version));
const apiHeaders = (extra = {}) => (tg?.initData ? { ...extra, 'X-Telegram-Init-Data': tg.initData } : extra);
const haptic = {
  // A light tick when a choice changes: tabs, toggles, scrubbing a chart.
  select: () => tgHas('6.1') && tg.HapticFeedback.selectionChanged(),
  // A save worked or failed; a rest is over.
  done: (ok = true) => tgHas('6.1') && tg.HapticFeedback.notificationOccurred(ok ? 'success' : 'error'),
};
const token = name => getComputedStyle(document.documentElement).getPropertyValue(name).trim();
// Fallback hexes matching styles.css: on Mini App reopen telegram-web-app.js is cached while
// styles.css is no-store, so app.js can run before styles apply and token() returns "".
// Telegram's colour setters throw on empty/invalid hex, which would abort the rest of app.js.
const TG_COLORS = {
  light: { paper: '#f1f1eb', white: '#fcfcf8', lime: '#d7f580', onLime: '#242824' },
  dark: { paper: '#121412', white: '#1b1e1b', lime: '#d7f580', onLime: '#242824' },
};
const tgColor = (cssVar, key) => token(cssVar) || TG_COLORS[document.documentElement.dataset.theme === 'dark' ? 'dark' : 'light'][key];
// Isolate Telegram SDK calls so a client quirk or unsupported call never aborts app startup.
const tgTry = fn => { try { fn(); } catch (e) { console.warn('[tg]', e); } };

/* Our palette, light or dark as Telegram is, with Telegram's header, background and bottom bar painted
   to match so the page reads as part of the app. */
function applyTelegramTheme() {
  document.documentElement.dataset.theme = tg.colorScheme === 'dark' ? 'dark' : 'light';
  if (tgHas('6.9')) tgTry(() => tg.setHeaderColor(tgColor('--paper', 'paper')));
  if (tgHas('6.1')) tgTry(() => tg.setBackgroundColor(tgColor('--paper', 'paper')));
  if (tgHas('7.10')) tgTry(() => tg.setBottomBarColor(tgColor('--white', 'white')));
}

/* Telegram's back button: closes the exercise sheet, else goes back to Today. */
function syncBack() {
  if (!tgHas('6.1')) return;
  tgTry(() => {
    if (document.getElementById('ex-detail').open || activeTab() !== 'today') tg.BackButton.show();
    else tg.BackButton.hide();
  });
}

/* Telegram's main button does the screen's one main thing: save an edited plan form while the exercise
   sheet is open, else log the live set on Today. In a browser each has its own button on the page
   (styles.css hides those in Telegram). An unsaved edit asks before the app closes. */
let dirtyForm = null;
let liveAction = null;
let mainRun = null;
function syncMain() {
  const sheet = document.getElementById('ex-detail').open;
  const action = sheet ? dirtyForm && { text: dirtyForm.dataset.label, run: () => dirtyForm.requestSubmit() }
    : activeTab() === 'today' ? liveAction : null;
  mainRun = action?.run || null;
  if (!tg) return;
  if (action) tgTry(() => tg.MainButton.setParams({
    text: action.text,
    color: tgColor('--lime', 'lime'),
    text_color: tgColor('--on-lime', 'onLime'),
    is_visible: true,
    is_active: !action.busy,
  }));
  else tgTry(() => tg.MainButton.hide());
  if (tgHas('6.2')) tgTry(() => (dirtyForm ? tg.enableClosingConfirmation() : tg.disableClosingConfirmation()));
}
function markDirty(form, label) { form.dataset.label = label; dirtyForm = form; syncMain(); }
function clearDirty() { dirtyForm = null; tg?.MainButton.hideProgress(); syncMain(); }

if (tg) {
  document.documentElement.classList.add('tg');
  applyTelegramTheme();
  tgTry(() => tg.onEvent('themeChanged', applyTelegramTheme));
  // On phones the page fills the screen: no header strip, Telegram's Close and ⋯ float over our own
  // background (styles.css keeps content clear of them). Desktop and older clients expand instead.
  if (tgHas('8.0') && ['ios', 'android'].includes(tg.platform)) {
    const syncFull = () => document.documentElement.classList.toggle('tg-full', Boolean(tg.isFullscreen));
    syncFull();
    tgTry(() => tg.onEvent('fullscreenChanged', syncFull));
    tgTry(() => tg.onEvent('fullscreenFailed', event => {
      syncFull();
      if (event?.error !== 'ALREADY_FULLSCREEN') tgTry(() => tg.expand());
    }));
    tgTry(() => tg.requestFullscreen());
  } else {
    tgTry(() => tg.expand());
  }
  // Scrubbing a chart would otherwise drag the whole app down; Close stays in Telegram's header.
  if (tgHas('7.7')) tgTry(() => tg.disableVerticalSwipes());
  if (tgHas('6.1')) tgTry(() => tg.BackButton.onClick(() => {
    const dialog = document.getElementById('ex-detail');
    if (dialog.open) closeSheet();
    else goTab('today');
  }));
  tgTry(() => tg.MainButton.onClick(() => mainRun?.()));
}

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined && text !== null) node.textContent = text;
  return node;
}

function svgEl(tag, attributes) {
  const node = document.createElementNS(SVG, tag);
  Object.entries(attributes).forEach(([key, value]) => node.setAttribute(key, String(value)));
  return node;
}

function svgText(x, y, text, attributes = {}) {
  const node = svgEl('text', { x, y, class: 'label', ...attributes });
  node.textContent = text;
  return node;
}

function ago(iso) {
  const minutes = Math.max(0, Math.round((now() - Date.parse(iso)) / 60000));
  if (minutes < 1) return 'just now';
  if (minutes < 60) return `${minutes} min ago`;
  return minutes < 1440 ? `${Math.floor(minutes / 60)} h ago` : `${Math.floor(minutes / 1440)} d ago`;
}

const clock = minutes => `${String(Math.floor(minutes / 60) % 24).padStart(2, '0')}:${String(Math.round(minutes % 60)).padStart(2, '0')}`;
const KG_PER_LB = 0.45359237;
const isLb = () => state?.unit === 'lb';
const weightUnit = () => isLb() ? 'lb' : 'kg';
const toDisplay = kg => kg === null || kg === undefined ? kg : isLb() ? Math.round((kg / KG_PER_LB) * 2) / 2 : kg;
const toDisplayBody = kg => kg === null || kg === undefined ? kg : isLb() ? Math.round((kg / KG_PER_LB) * 10) / 10 : kg;
const bodyWeightText = kg => `${Number(toDisplayBody(kg))} ${weightUnit()}`;
const toKg = v => v === null || v === undefined ? v : isLb() ? Math.round(v * KG_PER_LB * 100) / 100 : v;
const tonnes = kg => {
  if (isLb()) { const lb = kg / KG_PER_LB; return lb >= 1000 ? `${(lb / 1000).toFixed(1)}k lb` : `${Math.round(lb)} lb`; }
  return kg >= 1000 ? `${(kg / 1000).toFixed(1)} t` : `${Math.round(kg)} kg`;
};
const kgs = value => `${Number(toDisplay(value))} ${weightUnit()}`;

function crowdLabel(session) {
  if (session.crowd) return session.crowd;
  return session.count !== null && session.count !== undefined ? `~${session.count} ppl` : null;
}

/* An exercise picture. No picture (or one that fails to load) leaves an empty, quiet space, never a
   broken-image box. Lazy-loaded via IntersectionObserver with a tiny blurred silhouette placeholder
   (LQIP) painted immediately so slow Wi-Fi never shows blank boxes or layout pop-in. */
const previews = Object.create(null);
const loadedImgs = new Set();
function mergePreviews(map) {
  if (!map) return;
  for (const [url, uri] of Object.entries(map)) {
    if (url && uri) previews[url] = uri;
  }
}
function loadFullThumb(image) {
  const url = image.dataset.src;
  if (!url) return;
  delete image.dataset.src;
  const full = new Image();
  full.decoding = 'async';
  full.onload = () => {
    loadedImgs.add(url);
    image.src = url;
    image.classList.remove('img-lqip');
  };
  full.onerror = () => {
    image.classList.remove('img-lqip');
    image.dataset.missing = '';
  };
  full.src = url;
}
const imgObserver = typeof IntersectionObserver !== 'undefined'
  ? new IntersectionObserver(entries => {
    for (const entry of entries) {
      if (!entry.isIntersecting) continue;
      imgObserver.unobserve(entry.target);
      loadFullThumb(entry.target);
    }
  }, { rootMargin: '200px 0px' })
  : null;
function pruneImgObserver() {
  if (!imgObserver) return;
  imgObserver.disconnect();
  document.querySelectorAll('img[data-src]').forEach(img => imgObserver.observe(img));
}

function thumb(url) {
  const image = el('img');
  image.alt = '';
  image.decoding = 'async';
  if (!url) {
    image.dataset.missing = '';
    return image;
  }
  if (loadedImgs.has(url)) {
    image.addEventListener('error', () => { image.dataset.missing = ''; });
    image.src = url;
    return image;
  }
  const preview = previews[url];
  if (preview) {
    image.src = preview;
    image.classList.add('img-lqip');
    image.dataset.src = url;
    if (imgObserver) imgObserver.observe(image);
    else loadFullThumb(image);
    return image;
  }
  image.loading = 'lazy';
  image.addEventListener('load', () => loadedImgs.add(url));
  image.addEventListener('error', () => { image.dataset.missing = ''; });
  image.src = url;
  return image;
}

/* One tooltip for every chart. Values lead, labels follow. */
const tooltip = document.getElementById('tooltip');
function showTip(event, title, rows) {
  tooltip.replaceChildren(el('div', 'tip-title', title));
  for (const [value, label, kind] of rows) {
    const row = el('div', 'tip-row');
    if (kind) row.append(el('i', `tip-key ${kind}`));
    row.append(el('strong', '', value), el('span', '', label));
    tooltip.append(row);
  }
  tooltip.hidden = false;
  const box = tooltip.getBoundingClientRect();
  const left = Math.min(window.innerWidth - box.width - 8, event.clientX + 14);
  tooltip.style.left = `${Math.max(8, left) + window.scrollX}px`;
  tooltip.style.top = `${Math.max(8, event.clientY - box.height - 12) + window.scrollY}px`;
}
// The crosshair a tooltip belongs to, hidden with it.
let clearHover = null;
const hideTip = () => { tooltip.hidden = true; clearHover?.(); clearHover = null; };
/* Hover with a mouse; tap or drag with a finger. A touch tooltip stays after the finger lifts, until
   the next tap elsewhere, so it can be read. */
function hover(node, show, hide = hideTip) {
  // A finger starting a page scroll on a chart is a pointerdown too: touch shows on a tap (pointerup) or
  // a sideways drag (pointermove; a vertical one scrolls the page and cancels the pointer instead).
  node.addEventListener('pointermove', show);
  node.addEventListener('pointerdown', event => { if (event.pointerType !== 'touch') show(event); });
  node.addEventListener('pointerup', event => { if (event.pointerType === 'touch') show(event); });
  node.addEventListener('pointerleave', event => { if (event.pointerType !== 'touch') hide(); });
}
document.addEventListener('pointerdown', event => { if (!event.target.closest?.('.hit, .bar-row, .hm-cell')) hideTip(); });

/* Chart frame: an SVG sized to its container, with y gridlines. */
function frame(box, top, { height = 160, left = 30, label = 'Chart' } = {}) {
  box.replaceChildren();
  const W = Math.max(280, box.clientWidth || 520), H = height, L = left, R = 10, T = 10, B = 26;
  const svg = svgEl('svg', { viewBox: `0 0 ${W} ${H}`, role: 'img', 'aria-label': label });
  const y = value => T + (1 - value / top) * (H - T - B);
  for (const value of [0, top / 2, top]) {
    svg.append(svgEl('line', { x1: L, x2: W - R, y1: y(value), y2: y(value), class: 'grid' }));
    svg.append(svgText(L - 8, y(value) + 3, Number.isInteger(value) ? value : value.toFixed(1), { 'text-anchor': 'end' }));
  }
  box.append(svg);
  return { svg, W, H, L, R, T, B, y };
}

const niceTop = peak => peak <= 5 ? 5 : peak <= 10 ? 10 : Math.ceil(peak / 10) * 10;

/* Crosshair: the nearest data position to the pointer gets a hairline and a tooltip. */
function crosshair(chart, xs, describe) {
  const line = svgEl('line', { y1: chart.T, y2: chart.H - chart.B, class: 'crosshair', visibility: 'hidden' });
  const dot = svgEl('circle', { r: 4.5, class: 'hover-dot', visibility: 'hidden' });
  const hit = svgEl('rect', { x: chart.L, y: 0, width: chart.W - chart.L - chart.R, height: chart.H, class: 'hit' });
  chart.svg.append(line, dot, hit);
  let shown = -1;
  const clear = () => { line.setAttribute('visibility', 'hidden'); dot.setAttribute('visibility', 'hidden'); shown = -1; };
  hover(hit, event => {
    const rect = chart.svg.getBoundingClientRect();
    const px = (event.clientX - rect.left) * chart.W / rect.width;
    let index = 0;
    xs.forEach((x, i) => { if (Math.abs(x - px) < Math.abs(xs[index] - px)) index = i; });
    // A finger scrubbing across points ticks once per point.
    if (event.pointerType === 'touch' && index !== shown) haptic.select();
    shown = index;
    const info = describe(index);
    line.setAttribute('x1', xs[index]); line.setAttribute('x2', xs[index]); line.setAttribute('visibility', 'visible');
    if (info.y !== undefined) { dot.setAttribute('cx', xs[index]); dot.setAttribute('cy', info.y); dot.setAttribute('visibility', 'visible'); }
    showTip(event, info.title, info.rows);
    clearHover = clear;
  });
}

/* ---------- Today ---------- */

function renderWeek(days, range) {
  const week = document.getElementById('week');
  const prevScroll = week.scrollLeft;
  week.replaceChildren();
  for (const day of days) {
    const column = el('div', `day${day.today ? ' today' : ''}`);
    const heading = el('div', 'day-heading');
    heading.append(el('span', '', day.today ? 'Today' : day.name), el('strong', '', day.date));
    const session = day.session;
    const slot = el('div', 'slot');
    if (session) {
      slot.classList.add('training', dataClass('status', session.status));
      slot.append(el('b', '', session.start), el('span', '', session.template));
      const crowd = crowdLabel(session);
      if (crowd) slot.append(el('em', `crowd ${dataClass('crowd', session.crowd)}`, crowd));
    } else {
      slot.classList.add('rest');
      slot.setAttribute('aria-label', 'Rest day');
    }
    column.append(heading, slot);
    week.append(column);
  }
  week.scrollLeft = prevScroll;
  const count = days.filter(day => day.session).length;
  document.getElementById('range').textContent = range;
  document.getElementById('session-count').textContent = `${count} session${count === 1 ? '' : 's'}`;
}

/* ---------- Gym attendance ----------
   Times are local wall-clock minutes since 1970 (server-side, profile timezone), so a
   Date read with getUTC* gives local calendar fields without any timezone math. */
const DAY = 1440;
const crowdView = { view: 'today', back: 0 };
/* How far › pages ahead per view (negative `back`). The forecast is a typical-weekday curve, so
   a month out is as good as it gets; further only repeats the same weeks. */
const CROWD_AHEAD = { today: 7, '24h': 7, week: 4, month: 1 };
const local = minutes => new Date(minutes * 60000);
const dayStart = minutes => Math.floor(minutes / DAY) * DAY;
const weekStart = minutes => dayStart(minutes) - ((local(minutes).getUTCDay() + 6) % 7) * DAY;
const monthStart = (minutes, back) => { const d = local(minutes); return Date.UTC(d.getUTCFullYear(), d.getUTCMonth() - back, 1) / 60000; };
const dateLabel = (minutes, options) => local(minutes).toLocaleDateString('en-GB', { timeZone: 'UTC', ...options });
const dayLabel = minutes => dateLabel(minutes, { weekday: 'short', day: 'numeric', month: 'short' });

/* [start, end) of the selected window; `until` clips the axis (Today runs 0:00 → now). */
function crowdWindow(view, back, now) {
  if (view === '24h') return { start: now - (back + 1) * DAY, end: now - back * DAY };
  if (view === 'week') { const start = weekStart(now) - back * 7 * DAY; return { start, end: start + 7 * DAY }; }
  if (view === 'month') return { start: monthStart(now, back), end: monthStart(now, back - 1) };
  const start = dayStart(now) - back * DAY;
  return { start, end: back ? start + DAY : now };
}

function crowdTitle(view, back, { start, end }) {
  if (view === '24h') return back ? `24 h to ${dayLabel(end)} ${clock(end % DAY)}` : 'Last 24 h';
  if (view === 'week') return `${dateLabel(start, { day: 'numeric', month: 'short' })} – ${dateLabel(end - DAY, { day: 'numeric', month: 'short' })}`;
  if (view === 'month') return dateLabel(start, { month: 'long', year: 'numeric' });
  return back === 0 ? 'Today' : back === 1 ? 'Yesterday' : back === -1 ? 'Tomorrow' : dayLabel(start);
}

/* Typical day for `day`'s weekday: per hour, mean of each earlier same-weekday's mean (13 weeks). */
function typicalDay(series, day) {
  const hours = new Map();
  for (const [m, count] of series) {
    const back = day - dayStart(m);
    if (back <= 0 || back > 91 * DAY || back % (7 * DAY)) continue;
    const hour = Math.floor((m % DAY) / 60);
    const dates = hours.get(hour) || new Map();
    dates.set(back, [...(dates.get(back) || []), count]);
    hours.set(hour, dates);
  }
  const avg = values => values.reduce((a, b) => a + b, 0) / values.length;
  return [...hours.entries()].sort((a, b) => a[0] - b[0])
    .map(([hour, dates]) => [day + hour * 60 + 30, Math.round(avg([...dates.values()].map(avg)) * 10) / 10]);
}

/* Forecast for [start, end): each day's typical curve from today on. Later weeks reuse this
   week's curve for the weekday, so the forecast rests on the most recent history. */
function forecastSpan(series, start, end, now) {
  const today = dayStart(now), curves = new Map(), out = [];
  for (let day = dayStart(Math.max(start, now)); day < end; day += DAY) {
    const shift = Math.floor((day - today) / (7 * DAY)) * 7 * DAY;
    if (!curves.has(day - shift)) curves.set(day - shift, typicalDay(series, day - shift));
    out.push(...curves.get(day - shift).map(([m, v]) => [m + shift, v]));
  }
  return out.filter(([m]) => m >= start && m < end);
}

/* Month view averages readings per hour to keep the line readable. */
function hourlyMeans(points) {
  const hours = new Map();
  for (const [m, count] of points) { const h = Math.floor(m / 60) * 60; hours.set(h, [...(hours.get(h) || []), count]); }
  return [...hours.entries()].map(([h, values]) => [h + 30, Math.round(values.reduce((a, b) => a + b, 0) / values.length * 10) / 10]);
}

function axisTicks(view, start, end, width) {
  // Week labels sit mid-day; renderAttendance draws the midnight separators.
  if (view === 'week') return Array.from({ length: 7 }, (_, i) => [start + i * DAY + DAY / 2, dateLabel(start + i * DAY, { weekday: 'short', day: 'numeric' })]);
  if (view === 'month') {
    const ticks = [];
    for (let m = start; m < end; m += 7 * DAY) ticks.push([m, dateLabel(m, { day: 'numeric', month: 'short' })]);
    return ticks;
  }
  const hours = (end - start) / 60;
  const step = [1, 2, 3, 4, 6, 12].find(s => hours / s <= width / 55) || 12;
  const ticks = [];
  for (let m = Math.ceil(start / (step * 60)) * step * 60; m <= end; m += step * 60) ticks.push([m, clock(m % DAY)]);
  return ticks;
}

function renderAttendance(crowd) {
  const box = document.getElementById('chart');
  const series = crowd.series || [];
  const now = crowd.local_now;
  const { view, back } = crowdView;
  const window_ = crowdWindow(view, back, now);
  const { start, end } = window_;
  document.querySelectorAll('.crowd-nav .segmented button').forEach(button => button.setAttribute('aria-pressed', String(button.dataset.view === view)));
  document.getElementById('crowd-title').textContent = crowdTitle(view, back, window_);
  document.getElementById('crowd-prev').disabled = !series.length || series[0][0] >= start;
  document.getElementById('crowd-next').disabled = back <= -CROWD_AHEAD[view];
  const raw = series.filter(([m]) => m >= start && m < end);
  const points = view === 'month' ? hourlyMeans(raw) : raw;
  // Anything past now is forecast: the typical curve (day views) or each day's typical curve.
  const windowEnd = view === 'today' ? start + DAY : end;
  const ahead = windowEnd > now;
  const typical = view === 'today' ? typicalDay(series, start).filter(([m]) => m < windowEnd) : [];
  const later = !ahead ? [] : (view === 'today' ? typical : forecastSpan(series, start, end, now)).filter(([m]) => m > now);
  document.getElementById('legend-day').textContent = view === 'today' ? crowdTitle(view, back, window_) : 'Check-ins';
  const typicalKey = document.getElementById('legend-typical');
  typicalKey.hidden = typical.length < 2;
  typicalKey.textContent = `Typical ${dateLabel(start, { weekday: 'long' })}`;
  const stats = document.getElementById('crowd-stats');
  if (raw.length) {
    const peak = raw.reduce((best, point) => point[1] > best[1] ? point : best);
    const when = view === 'today' ? clock(peak[0] % DAY) : `${dayLabel(peak[0])} ${clock(peak[0] % DAY)}`;
    stats.textContent = `Peak ${peak[1]} at ${when} · ${raw.length} readings${view === 'month' ? ' · hourly averages' : ''}`;
  } else stats.textContent = '';
  // Summarize hours that haven't started yet (points sit mid-hour).
  const upcoming = later.filter(([m]) => m - 30 >= now);
  if (upcoming.length) {
    const busiest = upcoming.reduce((a, b) => b[1] > a[1] ? b : a), quietest = upcoming.reduce((a, b) => b[1] < a[1] ? b : a);
    const at = m => (view === 'today' ? '' : `${dayLabel(m)} `) + clock(m % DAY - 30);
    const forecast = `${start > now ? 'Expected' : 'Later'}: busiest ~${Math.round(busiest[1])} around ${at(busiest[0])}`
      + (quietest !== busiest ? `, quietest ~${Math.round(quietest[1])} around ${at(quietest[0])}` : '');
    stats.textContent = stats.textContent ? `${stats.textContent} · ${forecast}` : forecast;
  }
  if (!points.length && typical.length < 2 && later.length < 2) {
    box.replaceChildren(el('p', 'empty', 'No readings in this period.'));
    return;
  }
  const all = [...points, ...typical, ...later];
  const chart = frame(box, niceTop(Math.max(...all.map(p => p[1]))), { label: `Reported check-ins, ${crowdTitle(view, back, window_)}` });
  const axisEnd = Math.max(end, start + 60, later.length ? later.at(-1)[0] + 30 : 0);
  const x = m => chart.L + (m - start) / (axisEnd - start) * (chart.W - chart.L - chart.R);
  for (const [m, label] of axisTicks(view, start, axisEnd, chart.W)) chart.svg.append(svgText(x(m), chart.H - 6, label, { 'text-anchor': 'middle' }));
  if (view === 'week') for (let m = start + DAY; m < end; m += DAY) chart.svg.append(svgEl('line', { x1: x(m), x2: x(m), y1: chart.T, y2: chart.H - chart.B, class: 'grid' }));
  // Gaps longer than an hour (or two buckets) break the line instead of bridging it.
  const gap = view === 'month' ? 120 : 60;
  const runs = list => list.reduce((acc, point, i) => { if (!i || point[0] - list[i - 1][0] > gap) acc.push([]); acc.at(-1).push(point); return acc; }, []);
  const path = run => run.map(([m, v], i) => `${i ? 'L' : 'M'}${x(m).toFixed(1)},${chart.y(v).toFixed(1)}`).join(' ');
  for (const run of runs(typical)) if (run.length > 1) chart.svg.append(svgEl('path', { d: path(run), class: 'typical' }));
  if (later.length) {
    // Forecast: the typical curve right of now, shaded and drawn stronger via a clip at now.
    const clip = svgEl('clipPath', { id: 'crowd-ahead' });
    clip.append(svgEl('rect', { x: Math.max(0, x(now)), y: 0, width: chart.W, height: chart.H }));
    chart.svg.append(clip);
    for (const run of runs(view === 'today' ? typical : later)) if (run.length > 1 && run.at(-1)[0] > now) {
      chart.svg.append(svgEl('path', { d: `${path(run)} L${x(run.at(-1)[0])},${chart.y(0)} L${x(run[0][0])},${chart.y(0)} Z`, class: 'forecast-area', 'clip-path': 'url(#crowd-ahead)' }));
      chart.svg.append(svgEl('path', { d: path(run), class: 'forecast', 'clip-path': 'url(#crowd-ahead)' }));
    }
  }
  if (now >= start && now <= axisEnd && (ahead || later.length)) {
    chart.svg.append(svgEl('line', { x1: x(now), x2: x(now), y1: chart.T, y2: chart.H - chart.B, class: 'now' }));
    chart.svg.append(svgText(x(now), chart.T - 2, 'now', { 'text-anchor': 'middle', class: 'label now-label' }));
  }
  workoutMarkers(chart, x, crowd.sessions || [], { start, end: axisEnd, now, view });
  for (const run of runs(points)) {
    chart.svg.append(svgEl('path', { d: `${path(run)} L${x(run.at(-1)[0])},${chart.y(0)} L${x(run[0][0])},${chart.y(0)} Z`, class: 'area' }));
    chart.svg.append(svgEl('path', { d: path(run), class: 'line' }));
  }
  if (points.length && back === 0 && view !== 'month') {
    const [m, v] = points.at(-1);
    chart.svg.append(svgEl('circle', { cx: x(m), cy: chart.y(v), r: 4, class: 'dot' }));
  }
  const typicalAt = m => typical.find(([t]) => Math.floor(t / 60) === Math.floor(m / 60))?.[1];
  // Readings first, then the forecast hours past the last reading.
  const hover = points.length ? [...points, ...later.filter(([m]) => m > points.at(-1)[0])] : later.length ? later : typical;
  crosshair(chart, hover.map(([m]) => x(m)), index => {
    const [m, v] = hover[index];
    const reading = index < points.length;
    const time = view === 'month' ? `${clock(m % DAY - 30)}–${clock(m % DAY + 30)}` : clock(m % DAY);
    const title = view === 'today' ? (reading ? time : `${clock(m % DAY - 30)}–${clock(m % DAY + 30)}`) : `${dayLabel(m)} ${time}`;
    const rows = reading ? [[v, view === 'month' ? 'avg checked in' : 'checked in', 'today']] : [];
    const usual = typicalAt(m);
    if (usual !== undefined) rows.push([usual, `typical ${dateLabel(start, { weekday: 'long' })}`, 'typical']);
    else if (!reading) rows.push([v, 'expected', 'typical']);
    return { title, rows, y: chart.y(v) };
  });
}

/* Planned workouts as vertical lines like "now"; past/finished ones are quieter. Week/Month label
   with the initial only, and skip a label that would crowd the previous one. */
function workoutMarkers(chart, x, sessions, { start, end, now, view }) {
  const short = view === 'week' || view === 'month';
  let lastX = -Infinity;
  for (const { start: m, name, status } of sessions) {
    if (m < start || m > end) continue;
    const quiet = m < now || !['TENTATIVE', 'COMMITTED'].includes(status);
    const cls = `workout-mark${quiet ? ' quiet' : ''}`;
    chart.svg.append(svgEl('line', { x1: x(m), x2: x(m), y1: chart.T, y2: chart.H - chart.B, class: cls }));
    const label = short ? String(name).slice(0, 1) : name;
    if (x(m) - lastX < (short ? 10 : 36)) continue;
    lastX = x(m);
    // "now" and a session label at the same spot read "nowPush": the session wins, the line still marks now.
    if (now >= start && now <= end && Math.abs(x(m) - x(now)) < 36) chart.svg.querySelector('.now-label')?.remove();
    chart.svg.append(svgText(x(m), chart.T - 2, label, { 'text-anchor': 'middle', class: `label ${cls}` }));
  }
}

function renderGym(crowd) {
  const panel = document.querySelector('.gym-panel');
  const empty = !crowd.polling && !(crowd.series && crowd.series.length);
  if (panel) panel.hidden = empty;
  if (empty) { document.getElementById('polling-alert').hidden = true; return; }
  const health = crowd.health || { status: 'unknown' };
  document.getElementById('count').textContent = crowd.now ? crowd.now.count : '—';
  document.getElementById('count-time').textContent = crowd.now ? `${crowd.now.time} · ${ago(crowd.now.at)}` : 'No readings yet';
  const polling = document.getElementById('polling');
  const broken = crowd.polling && (health.status === 'stale' || health.status === 'failing');
  polling.textContent = !crowd.polling ? 'Polling off' : broken ? 'Not updating' : 'Every 15 min';
  polling.className = broken ? 'warn' : '';
  const alert = document.getElementById('polling-alert');
  alert.hidden = !broken;
  if (broken) alert.textContent = `Gym check-ins stopped updating ${ago(health.last_success_at)}${health.error ? ` (gym API: ${health.error})` : ''}. Crowd forecasts are going stale.`;
  renderAttendance(crowd);
}

function renderActivity(activity, today) {
  const list = document.getElementById('events');
  list.replaceChildren();
  if (!activity.length) list.append(el('p', 'empty', 'Nothing yet.'));
  // Repeats in a row collapse into one line ("Set logged ×3").
  const grouped = [];
  for (const event of activity) {
    const last = grouped.at(-1);
    if (last && last.text === event.text) last.count += 1;
    else grouped.push({ ...event, count: 1 });
  }
  for (const event of grouped.slice(0, 7)) {
    const row = el('div', 'event');
    row.append(el('time', '', event.day === today ? event.time : `${event.day} ${event.time}`), el('span', 'event-dot'), el('span', '', event.count > 1 ? `${event.text} ×${event.count}` : event.text));
    list.append(row);
  }
}

function startRest(node, until) {
  clearInterval(restTimer);
  let counting = false;
  const tick = () => {
    const left = Math.max(0, Math.ceil((Date.parse(until) - now()) / 1000));
    node.textContent = left ? `Rest ${Math.floor(left / 60)}:${String(left % 60).padStart(2, '0')}` : 'Rest over';
    // Buzz only when the rest runs out on screen, not when the page opens after it.
    if (!left) {
      clearInterval(restTimer);
      // Over: no −15s on a rest that's done; the logger comes into view for the set.
      node.closest('.resting')?.querySelector('.rest-actions')?.remove();
      if (counting) { haptic.done(); toast('Rest over · next set'); document.querySelector('.logger')?.scrollIntoView({ block: 'nearest', behavior: 'smooth' }); }
    }
    counting = left > 0;
  };
  tick();
  restTimer = setInterval(tick, 1000);
}

/* A number with − and + on either side: big targets for a sweaty thumb, and a tap on the number types
   it (decimal keypad, no spinner arrows). Every valid keystroke counts at once, so a main-button tap
   right after typing uses the typed number. `nullable`: emptying it means "the default" (null). */
function stepper(label, value, { step = 1, min = 0, unit = '', base = 90, nullable = false, decimal = false, onChange }) {
  const row = el('div', 'stepper');
  const input = el('input');
  // The keypad follows what the number can be (12.5 kg on a dumbbell stepped by 2), not the step.
  Object.assign(input, { type: 'text', inputMode: decimal || step % 1 ? 'decimal' : 'numeric', enterKeyHint: 'done', autocomplete: 'off',
    placeholder: nullable ? 'default' : '—' });
  input.setAttribute('aria-label', label);
  const show = v => { input.value = v === null || v === undefined ? '' : String(v); };
  show(value);
  const commit = v => {
    const whole = !decimal && step % 1 === 0;
    value = v === null ? null : Math.max(min, whole ? Math.round(v) : Math.round(v * 100) / 100);
    onChange(value);
  };
  const typed = () => {
    const text = input.value.trim().replace(',', '.');
    if (text === '') return nullable ? null : undefined;
    const number = Number(text);
    return Number.isFinite(number) ? number : undefined;
  };
  const button = (text, delta, name) => {
    const b = el('button', 'step', text);
    b.type = 'button';
    b.setAttribute('aria-label', `${name} ${label.toLowerCase()}`);
    const bump = () => {
      haptic.select();
      // A blank (the server's default) steps from `base`; going under the minimum of a nullable one blanks it.
      const next = value === null || value === undefined ? base : Number(value) + delta;
      commit(nullable && next < Math.max(min, step) ? null : next);
      show(value);
    };
    // Tap: one step (on click, so a thumb that lands here to scroll changes nothing). Hold: keeps stepping,
    // so 100 → 120 kg is one press, not eight; moving the finger (a scroll) cancels it.
    let hold, held = false, startY = 0;
    const stop = () => { clearTimeout(hold); clearInterval(hold); };
    b.addEventListener('pointerdown', event => {
      if (event.button) return;
      held = false;
      startY = event.clientY;
      hold = setTimeout(() => { held = true; bump(); hold = setInterval(bump, 110); }, 450);
    });
    b.addEventListener('pointermove', event => { if (Math.abs(event.clientY - startY) > 10) stop(); });
    ['pointerup', 'pointerleave', 'pointercancel'].forEach(type => b.addEventListener(type, stop));
    b.addEventListener('click', () => { if (!held) bump(); held = false; });
    return b;
  };
  // Select on the next frame: select() right in focus pops iOS's cut/copy/paste bubble over the number.
  input.addEventListener('focus', () => requestAnimationFrame(() => input.setSelectionRange(0, input.value.length)));
  input.addEventListener('keydown', event => { if (event.key === 'Enter') input.blur(); });
  // What's in the box is what counts: emptied or under the minimum means "nothing yet", and the Log button
  // says what's missing. Never put back on blur: the blur comes from tapping Log, which would then log the
  // put-back value.
  const valid = v => (v === null ? nullable : v !== undefined && v >= min);
  input.addEventListener('input', () => {
    const v = typed();
    if (valid(v)) commit(v);
    else { value = null; onChange(null); }
  });
  input.addEventListener('change', () => {
    if (!valid(typed())) {
      // Not a number: say so (red flash, error buzz); the box empties and the button asks for it.
      row.classList.remove('invalid');
      void row.offsetWidth;
      row.classList.add('invalid');
      haptic.done(false);
    }
    show(value);
  });
  const field = el('label', 'step-value');
  field.append(input, el('span', '', unit));
  row.append(el('span', 'step-label', label), button('−', -step, 'Less'), field, button('+', step, 'More'));
  return row;
}

/* Small note at the bottom: what just happened. */
let toastTimer;
function toast(text, failed = false) {
  const node = document.getElementById('toast');
  node.textContent = text;
  node.classList.toggle('failed', failed);
  node.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { node.hidden = true; }, 4500);
}

/* A coach button from the app: the server runs it like a tap in the chat (`coach::app_tap`), and the bot
   moves the chat's card on as well. A button the workout has moved past is refused and the view refreshes. */
async function coachTap(data) {
  return send('/api/coach', { data }, body => body?.ack);
}

/* POST, then a fresh state. Buttons and the main button wait meanwhile; the outcome buzzes and toasts. */
let sending = false;
async function send(url, payload, said) {
  // One at a time: a double tap on the main button would otherwise log the set twice.
  if (sending) return false;
  sending = true;
  let ok = false;
  document.activeElement?.blur?.();
  if (liveAction) { liveAction.busy = true; syncMain(); }
  tg?.MainButton.showProgress();
  document.querySelectorAll('.focus button').forEach(b => { b.disabled = true; });
  try {
    // No connection (a dead zone): say what didn't happen, not the browser's "Failed to fetch".
    const response = await fetch(url, { method: 'POST', headers: apiHeaders({ 'Content-Type': 'application/json' }), body: JSON.stringify(payload) })
      .catch(() => { throw new Error('No connection. Check the last set before trying again.'); });
    const body = await response.json().catch(() => null);
    if (!response.ok) throw new Error(body?.error?.message?.split('\n')[0] || 'Didn\'t go through. Try again.');
    ok = true;
    haptic.done();
    const text = said(body);
    if (text) toast(text);
  } catch (error) {
    haptic.done(false);
    toast(error.message, true);
  } finally {
    tg?.MainButton.hideProgress();
    rendered = '';
    sending = false;
    // Offline the refresh fails too and nothing redraws: give the buttons back so "try again" can be tapped.
    if (!await refresh()) {
      document.querySelectorAll('.focus button').forEach(b => { b.disabled = false; });
      if (liveAction) liveAction.busy = false;
      syncMain();
    }
    // The card moved on (next set, rest, next exercise): bring its top into view.
    const focus = document.getElementById('focus');
    if (focus.getBoundingClientRect().top < 0) focus.scrollIntoView({ block: 'start' });
  }
  return ok;
}

// Chat labels carry emoji (✅ ⏭ 🔄) that some phones lack; the app shows the words.
const label = text => text.replace(/^[^\p{L}\p{N}+−-]+/u, '');

// Plates per side for a barbell weight on a 20 kg bar: "20 + 10 + 2.5". Empty when it doesn't add up.
function plates(weight) {
  if (isLb()) return '';
  let left = (weight - 20) / 2;
  if (left <= 0) return '';
  const out = [];
  for (const plate of [25, 20, 15, 10, 5, 2.5, 1.25]) while (left >= plate - 1e-9) { out.push(plate); left -= plate; }
  return left > 1e-9 ? '' : out.join(' + ');
}

// What the logger shows, kept while the same set is on screen so a refresh doesn't undo a change.
const logger = { key: null, weight: null, reps: null, seconds: null, meters: null, fixing: false };

// Estimated 1-rep max (Epley) to the nearest 0.5 kg, as the server computes it (`state::one_rep_max`).
const oneRepMax = (weight, reps) => reps > 1 ? Math.round(weight * (1 + reps / 30) * 2) / 2 : weight;
// The Progress series of the exercise on screen (its plan exercise id), if it has history.
const progressOf = active => {
  const id = state?.workout?.exercises.find(e => e.id === active?.id)?.exercise_id;
  return state?.insights.progress.find(s => s.id === id);
};

// Steps the gym's weights come in: barbell plates in pairs (2.5), dumbbells by 2, light stacks by 1.
const weightStep = (equipment, weight) => isLb() ? 5 : equipment === 'Barbell' ? 2.5 : equipment === 'Dumbbell' ? 2 : weight >= 20 ? 2.5 : 1;

/* The set logger: reps × weight, a time or a distance, starting from the chat card's suggestion. Logging
   is the main button (Telegram's, or the page's own in a browser). Fixing, it corrects `last` (its own
   exercise's units, which may not be the current one's). `active` is null when only fixing (a finished
   workout). */
function renderLogger(focus, live, active, last) {
  const fixing = Boolean(logger.fixing && last);
  const spec = fixing ? { ...last, measure: last.seconds ? 'time' : last.meters ? 'distance' : 'reps', reps_only: !last.weight } : live.logger;
  const equipment = fixing ? last.equipment : live.equipment;
  const key = fixing ? `fix:${last.id}` : `${active.id}:${active.set_number}:${active.set_type}`;
  if (logger.key !== key) Object.assign(logger, { key, weight: spec.weight !== null && spec.weight !== undefined ? toDisplay(spec.weight) : null, reps: spec.reps ?? null, seconds: spec.seconds ?? null, meters: spec.meters ?? null });
  const box = el('div', `logger${fixing ? ' fixing' : ''}`);
  if (fixing) box.append(el('p', 'logger-note', `Fixing ${last.exercise} · ${last.text}`));
  const measure = spec.measure;
  let words;
  if (measure === 'time') {
    // Minutes for cardio (and an open time), seconds for a plank.
    const long = (spec.seconds ?? 1200) >= 120;
    box.append(stepper(long ? 'Time' : 'Seconds', logger.seconds === null ? null : long ? logger.seconds / 60 : logger.seconds,
      { step: long ? 1 : 5, min: long ? 1 : 5, base: long ? 20 : 30, unit: long ? 'min' : 's', decimal: long,
        onChange: v => { logger.seconds = v === null ? null : Math.round(long ? v * 60 : v); refreshLabel(); } }));
    words = () => logger.seconds ? duration(logger.seconds) : null;
  } else if (measure === 'distance') {
    const km = (spec.meters ?? 1000) >= 1000;
    box.append(stepper('Distance', logger.meters === null ? null : km ? logger.meters / 1000 : logger.meters,
      { step: km ? 0.1 : 50, min: km ? 0.1 : 50, base: km ? 2 : 400, unit: km ? 'km' : 'm', decimal: km,
        onChange: v => { logger.meters = v === null ? null : Math.round(km ? v * 1000 : v); refreshLabel(); } }));
    words = () => logger.meters ? (logger.meters >= 1000 ? `${logger.meters / 1000} km` : `${logger.meters} m`) : null;
  } else {
    // Bodyweight hides the weight, except where weight is added (dip belt), even at 0.
    if (!spec.reps_only || spec.added || logger.weight) {
      box.append(stepper(spec.added ? 'Added' : 'Weight', logger.weight, { step: weightStep(equipment, logger.weight), base: 20, unit: spec.unit, decimal: true, min: spec.added ? 0 : 0.5,
        onChange: v => { logger.weight = v; refreshLabel(); } }));
      box.plates = el('p', 'plates');
      box.append(box.plates);
    }
    if (!spec.added && (spec.unit === 'kg' || spec.unit === 'lb')) { box.best = el('p', 'best'); }
    box.append(stepper(spec.per_side ? 'Reps / side' : 'Reps', logger.reps, { min: 1, base: 8, unit: 'reps', onChange: v => { logger.reps = v; refreshLabel(); } }));
    if (box.best) box.append(box.best);
    words = () => {
      // Added weight: 0 is an unweighted set, an empty box is missing. A lift needs a weight.
      if (!logger.reps || (spec.added ? logger.weight === null && !spec.reps_only : !spec.reps_only && !logger.weight)) return null;
      if (!logger.weight) return `${logger.reps} reps`;
      return `${logger.reps} × ${spec.added ? '+' : ''}${logger.weight} ${spec.unit}`;
    };
  }
  const submit = el('button', 'save log-button');
  submit.type = 'button';
  const missing = () => measure !== 'reps' ? measure : !logger.reps && (spec.reps_only || logger.weight) ? 'reps' : 'weight';
  const run = () => {
    // Nothing to log yet (a new exercise's weight, an open time): say what's missing instead of sending it.
    if (!words()) { toast(`Enter the ${missing()} first`, true); haptic.done(false); return; }
    if (fixing) {
      const fix = measure === 'time' ? { minutes: logger.seconds / 60 } : measure === 'distance' ? { meters: logger.meters } : { weight: toKg(logger.weight || 0), reps: logger.reps };
      logger.fixing = false;
      send('/api/set_edit', { set_id: last.id, ...fix }, () => `Fixed: ${words()}`);
      return;
    }
    const data = measure === 'time' ? `gc:time:${live.ref}:${logger.seconds}` : measure === 'distance' ? `gc:dist:${live.ref}:${logger.meters}` : `gc:log:${live.ref}:${toKg(logger.weight || 0)}:${logger.reps}`;
    coachTap(data);
  };
  function refreshLabel() {
    const what = words();
    // The set number changes with every log, so the button itself shows the last tap went through.
    const set = active && active.sets > 1 ? `set ${active.set_number} · ` : '';
    const text = !what ? `Enter the ${missing()}` : fixing ? `Fix to ${what}` : `Log ${set}${what}`;
    submit.textContent = text.charAt(0).toUpperCase() + text.slice(1);
    if (box.plates) box.plates.textContent = equipment === 'Barbell' && plates(logger.weight) ? `Each side: ${plates(logger.weight)}` : '';
    if (box.best) {
      // About to beat the best estimated 1RM: say so before the set, not only in the chat's confetti after.
      const best = Math.max(0, ...(progressOf(active)?.points || []).map(p => p.e1rm));
      const next = what ? oneRepMax(toKg(logger.weight || 0), logger.reps || 0) : 0;
      box.best.textContent = !fixing && best && next > best ? `New best: ≈ ${kgs(next)} 1RM (was ${kgs(best)})` : '';
    }
    liveAction = { text: submit.textContent, run };
    syncMain();
  }
  submit.addEventListener('click', run);
  box.append(submit);
  if (fixing) {
    const cancel = el('button', 'link', 'Cancel');
    cancel.type = 'button';
    cancel.addEventListener('click', () => { logger.fixing = false; logger.key = null; rerender(); });
    box.append(cancel);
  }
  focus.append(box);
  refreshLabel();
}

// Draw the current state again (after a local change such as opening the fixer).
const rerender = () => { rendered = ''; render(state); };

/* "Last set 9 × 87.5 kg · Fix": naming the exercise when it isn't the one on screen. Hidden while fixing. */
function lastLine(last, activeId) {
  const line = el('p', 'subtitle last');
  line.append(`Last set ${last.workout_exercise_id === activeId ? '' : `(${last.exercise}) `}${last.text} `);
  const fix = el('button', 'link', 'Fix');
  fix.type = 'button';
  fix.addEventListener('click', () => { logger.fixing = true; logger.key = null; rerender(); });
  line.append(fix);
  return line;
}

function actionButton(b, primary = false) {
  const button = el('button', primary ? 'save log-button' : 'chip', label(b.text));
  button.type = 'button';
  button.addEventListener('click', () => coachTap(b.data));
  return button;
}

/* Live workout: what to do now, entered and logged right here. The buttons are the chat card's own
   (same effect, same words), and every step shows up in the chat too. */
let queueActiveId = null;
function scrollQueueToCurrent(queue, activeId, prevScroll = null) {
  if (!queue) return;
  if (activeId && queueActiveId === activeId && prevScroll !== null) {
    queue.scrollLeft = prevScroll;
    return;
  }
  const current = queue.querySelector('.exercise.current');
  if (!current || queue.scrollWidth <= queue.clientWidth) return;
  const qRect = queue.getBoundingClientRect();
  const iRect = current.getBoundingClientRect();
  const target = queue.scrollLeft + (iRect.left - qRect.left) - (queue.clientWidth - iRect.width) / 2;
  queue.scrollLeft = Math.max(0, Math.min(target, queue.scrollWidth - queue.clientWidth));
  queueActiveId = activeId;
}

function renderWorkout(focus, workout, prevQueueScroll = null) {
  const active = workout.active;
  const live = workout.live || { buttons: [] };
  const buttons = live.buttons;
  const done = workout.exercises.filter(e => e.status === 'COMPLETED').length;
  const heading = el('div', 'section-heading');
  heading.append(el('span', '', `${workout.template} · exercise ${Math.min(done + 1, workout.exercises.length)} of ${workout.exercises.length}`), el('span', '', `Finish ~${workout.eta}`));
  focus.append(heading);
  liveAction = null;
  if (!active) {
    // Only machines that were taken are left; the chat asks the same.
    focus.append(el('h2', 'title', live.waiting ? `Only ${live.waiting} left` : 'Wrapping up'));
    if (live.waiting) focus.append(el('p', 'subtitle', 'It was taken earlier. Free now?'));
    const actions = el('div', 'live-actions');
    buttons.forEach((b, i) => actions.append(actionButton(b, i === 0)));
    if (buttons[0]) liveAction = { text: label(buttons[0].text), run: () => coachTap(buttons[0].data) };
    focus.append(actions);
    if (live.swap) renderSwap(focus, live.swap, true);
  } else {
    const warmup = active.set_type === 'WARMUP';
    const head = el('div', 'live-head');
    const titles = el('div');
    titles.append(el('h2', 'title', active.name), el('p', 'subtitle', `${warmup ? 'Warm-up' : `Set ${active.set_number} of ${active.sets}`} · ${live.target}`));
    // The last set, with a way to fix a mistyped one.
    if (workout.last && !logger.fixing) titles.append(lastLine(workout.last, active.id));
    const pose = el('button', 'pose');
    pose.type = 'button';
    pose.setAttribute('aria-label', `${active.name}: details`);
    pose.append(thumb(workout.exercises.find(e => e.id === active.id)?.svg_url));
    const current = workout.exercises.find(e => e.id === active.id);
    pose.addEventListener('click', () => current && openExercise({ id: current.exercise_id, name: current.name, svg_url: current.svg_url }));
    head.append(titles, pose);
    focus.append(head);
    const used = new Set();
    buttons.filter(b => /^gc:(cut|skip):/.test(b.data)).forEach(b => used.add(b));
    if (workout.rest_until && Date.parse(workout.rest_until) > now()) {
      // Resting: the countdown and the chat's ways to shorten it. The logger below is for the set after.
      const rest = el('div', 'status resting');
      const time = el('strong');
      // A timer, not a live region: a reader isn't read every second; "Rest over" arrives as a toast.
      time.setAttribute('role', 'timer');
      rest.append(time);
      const actions = el('div', 'rest-actions');
      for (const b of buttons.filter(b => /^gc:(cut|skip):/.test(b.data))) { used.add(b); actions.append(actionButton(b)); }
      rest.append(actions);
      startRest(time, workout.rest_until);
      focus.append(rest);
    }
    if (warmup && !logger.fixing) {
      // Warm-up: done or skipped, one tap each.
      const actions = el('div', 'live-actions');
      buttons.filter(b => /^gc:(warm|nowarm):/.test(b.data)).forEach((b, i) => {
        used.add(b);
        actions.append(actionButton(b, i === 0));
        if (i === 0) liveAction = { text: label(b.text), run: () => coachTap(b.data) };
      });
      focus.append(actions);
    } else {
      buttons.filter(b => /^gc:(log|time|dist):/.test(b.data)).forEach(b => used.add(b));
      renderLogger(focus, live, active, workout.last);
    }
    // Machine taken, before the first set: swap picker + "Do it later" / "Skip".
    if (live.swap && !logger.fixing) {
      buttons.filter(b => /^gc:(later|next):/.test(b.data) && (!current?.done || b.data.startsWith('gc:later:'))).forEach(b => used.add(b));
      renderSwap(focus, live.swap, false);
    }
    const rest = buttons.filter(b => !used.has(b));
    if (rest.length && !logger.fixing) {
      const actions = el('div', 'live-actions');
      rest.forEach(b => actions.append(actionButton(b)));
      focus.append(actions);
    }
  }
  if (freshState && pendingSwapRef) {
    const matches = live.swap?.ref === pendingSwapRef;
    pendingSwapRef = null;
    history.replaceState(null, '', `${location.pathname}${location.hash}`);
    if (matches) openSwapPicker(live.swap);
    else {
      closeAfterSwap = false;
      toast('That exercise has moved on.');
    }
  }
  const queue = el('div', 'queue');
  for (const exercise of workout.exercises) {
    const item = el('button', `exercise ${dataClass('status', exercise.status)}${exercise.id === active?.id ? ' current' : ''}`);
    item.type = 'button';
    item.append(thumb(exercise.svg_url), el('span', '', exercise.name), el('small', '', `${exercise.done}/${exercise.sets}`));
    item.setAttribute('aria-label', `${exercise.name}, ${exercise.done} of ${exercise.sets} sets${exercise.id === active?.id ? ', now' : ''}`);
    // Its details, and "Do it now" for one still to do (a machine just came free).
    item.addEventListener('click', () => openExercise({ id: exercise.exercise_id, name: exercise.name, svg_url: exercise.svg_url, now: live.queue?.[exercise.id] }));
    queue.append(item);
  }
  focus.append(queue);
  scrollQueueToCurrent(queue, active?.id ?? null, prevQueueScroll);
  // The rest of the app, one tap away; the tab bar steps aside mid-workout in Telegram.
  const more = el('nav', 'live-more');
  for (const [tab, name] of [['progress', 'Progress'], ['history', 'History'], ['exercises', 'Exercises'], ['memory', 'Settings']]) {
    const link = el('button', 'link', name);
    link.type = 'button';
    link.addEventListener('click', () => goTab(tab));
    more.append(link);
  }
  const end = el('button', 'link danger', 'End workout');
  end.type = 'button';
  end.addEventListener('click', () => {
    const ask = 'End this workout now and save logged sets?';
    const endData = `gc:end:${String(workout.id || '').slice(0, 8)}`;
    if (tgHas('6.2')) tg.showConfirm(ask, ok => ok && coachTap(endData));
    else if (window.confirm(ask)) coachTap(endData);
  });
  more.append(end);
  focus.append(more);
  syncMain();
}

/* Machine taken: open the tiered swap picker sheet, or defer / skip right from the live card. */
let pendingSwapRef = new URLSearchParams(location.search).get('swap');
let closeAfterSwap = Boolean(pendingSwapRef);

function renderSwap(focus, swap, waitingOnly) {
  const row = el('div', 'live-actions swap-bar');
  const open = el('button', 'chip swap-open', waitingOnly ? 'Swap for another exercise' : 'Swap exercise');
  open.type = 'button';
  open.addEventListener('click', () => {
    haptic.select();
    closeAfterSwap = false;
    openSwapPicker(swap);
  });
  row.append(open);
  if (!waitingOnly) {
    if (swap.later) row.append(actionButton({ text: 'Do it later', data: swap.later }));
    else if (swap.skip) row.append(actionButton({ text: 'Skip', data: swap.skip }));
  }
  focus.append(row);
}

/* Tiered swap picker sheet: top picks → same primary muscle → broader/related → full catalog search. */
function openSwapPicker(swap) {
  const dialog = document.getElementById('ex-detail');
  const sheet = el('div', 'ex-sheet swap-sheet');
  dialog.replaceChildren(sheet);
  const close = el('button', 'ex-close', '×');
  close.type = 'button';
  close.setAttribute('aria-label', 'Close');
  close.addEventListener('click', closeSheet);

  const head = el('div', 'swap-head');
  const title = el('h2', 'title', `Swap ${swap.exercise || 'exercise'}`);
  title.id = 'ex-detail-title';
  head.append(title);
  const meta = [swap.muscle, swap.equipment].filter(Boolean).join(' · ');
  if (meta) head.append(el('p', 'subtitle', meta));
  sheet.append(close, head);

  const pick = async data => {
    haptic.select();
    dialog.close();
    const shouldClose = closeAfterSwap && Boolean(tg);
    closeAfterSwap = false;
    const ok = await coachTap(data);
    if (shouldClose && ok) tgTry(() => tg.close());
  };

  if (swap.later || swap.skip) {
    const quick = el('div', 'live-actions swap-quick');
    if (swap.later) {
      const b = el('button', 'chip', 'Do it later');
      b.type = 'button';
      b.addEventListener('click', () => pick(swap.later));
      quick.append(b);
    }
    if (swap.skip) {
      const b = el('button', 'chip', 'Skip exercise');
      b.type = 'button';
      b.addEventListener('click', () => pick(swap.skip));
      quick.append(b);
    }
    sheet.append(quick);
  }

  let query = '';
  let equipment = null;
  let showAll = false;

  const search = el('input', 'swap-search');
  search.type = 'search';
  search.placeholder = 'Search any exercise or muscle…';
  search.setAttribute('aria-label', 'Search exercises');
  sheet.append(search);

  const chips = el('div', 'chips swap-chips');
  sheet.append(chips);

  const body = el('div', 'swap-body');
  sheet.append(body);

  const usedIds = new Set(swap.used || []);
  const kind = eq => (EQUIPMENT.includes(eq) ? eq : 'Other');

  function renderChips() {
    chips.replaceChildren(...[null, ...EQUIPMENT, 'Other'].map(value => {
      const chip = el('button', 'chip', value || 'All');
      chip.type = 'button';
      chip.setAttribute('aria-pressed', String(equipment === value));
      chip.addEventListener('click', () => {
        haptic.select();
        equipment = value;
        renderChips();
        renderBody();
      });
      return chip;
    }));
  }

  function makeTile(option, showMuscle) {
    const tile = el('button', 'ex-tile');
    tile.type = 'button';
    tile.append(thumb(option.svg_url), el('span', '', option.name));
    const sub = showMuscle
      ? [option.muscle, option.equipment].filter(Boolean).join(' · ')
      : option.equipment || '';
    if (sub) tile.append(el('small', '', sub));
    if (option.source === 'workout') tile.append(el('em', 'tag', 'Back to it'));
    else if (option.source === 'template') tile.append(el('em', 'tag', 'In plan'));
    tile.addEventListener('click', () => pick(option.data));
    return tile;
  }

  function renderTier(heading, note, items, showMuscle) {
    if (!items.length) return;
    const section = el('section', 'swap-tier');
    const titleRow = el('div', 'section-heading');
    titleRow.append(el('h3', '', heading), el('span', '', note));
    const grid = el('div', 'swap-grid');
    for (const item of items) grid.append(makeTile(item, showMuscle));
    section.append(titleRow, grid);
    body.append(section);
  }

  function renderBody() {
    body.replaceChildren();
    const q = query.trim().toLowerCase();
    const matches = item => {
      if (equipment && kind(item.equipment) !== equipment) return false;
      if (!q) return true;
      return [item.name, item.muscle, item.equipment, ...(item.secondary_muscles || [])]
        .some(t => String(t || '').toLowerCase().includes(q));
    };

    const top = (swap.top || []).filter(matches);
    const same = (swap.same || []).filter(matches);
    const related = (swap.related || []).filter(matches);

    renderTier('Top picks', `${top.length}`, top, false);
    renderTier(swap.muscle ? `Same muscle · ${swap.muscle}` : 'Same muscle', `${same.length}`, same, false);
    renderTier('Broader & related', `${related.length}`, related, true);

    const shown = new Set([...(swap.top || []), ...(swap.same || []), ...(swap.related || [])].map(o => o.id));
    if (q || showAll) {
      if (!catalog) {
        loadCatalog().then(() => dialog.open && renderBody());
        body.append(el('p', 'empty', 'Loading catalog…'));
        return;
      }
      const other = catalog
        .filter(i => !usedIds.has(i.guide_id) && !shown.has(i.guide_id))
        .filter(matches)
        .map(i => ({
          id: i.guide_id,
          name: i.name,
          muscle: i.muscle,
          equipment: i.equipment,
          svg_url: i.svg_url,
          data: `gc:sub:${swap.ref}:${i.guide_id}`,
        }));
      renderTier('All other exercises', `${other.length}`, other, true);
    } else {
      const more = el('button', 'link swap-more', 'Browse all exercises');
      more.type = 'button';
      more.addEventListener('click', () => {
        haptic.select();
        showAll = true;
        renderBody();
      });
      body.append(more);
    }

    if (!body.querySelector('.ex-tile')) {
      body.append(el('p', 'empty', q ? `Nothing matches “${query.trim()}”.` : 'No matches for this equipment filter.'));
    }
  }

  search.addEventListener('input', () => {
    query = search.value;
    renderBody();
  });

  renderChips();
  renderBody();
  if (!dialog.open) dialog.showModal();
  syncBack();
  syncMain();
}

function renderNext(focus, next) {
  const heading = el('div', 'section-heading');
  // "Leave 17:35" only means something today; another day just says when.
  const when = next.today ? `Today ${next.when.split(' ')[1]}` : next.when;
  heading.append(el('span', '', `Next · ${when}`), el('span', '', next.today ? `Leave ${next.leave}` : ''));
  focus.append(heading, el('h2', 'title', next.template));
  // At the gym: start it here, the same as the reminder's Start button.
  if (next.begin) {
    const start = el('button', 'save log-button', `Start ${next.template}`);
    start.type = 'button';
    const run = () => coachTap(next.begin);
    start.addEventListener('click', run);
    liveAction = { text: `Start ${next.template}`, run };
    focus.append(start);
  }
  const crowd = crowdLabel(next);
  if (crowd) focus.append(el('p', 'subtitle', next.crowd && next.count ? `${next.crowd} · ~${next.count} people expected` : `${crowd} expected`));
  const list = el('ol', 'plan');
  for (const exercise of next.exercises) {
    const row = el('li');
    const button = el('button');
    button.type = 'button';
    const text = el('div');
    text.append(el('strong', '', exercise.name), el('span', '', exercise.time ? (exercise.sets > 1 ? `${exercise.sets} × ${exercise.time}` : exercise.time) : exercise.sets > 1 ? `${exercise.sets} × ${exercise.target}` : exercise.target));
    button.append(thumb(exercise.svg_url), text, el('em', '', exercise.last ? `Last ${exercise.last}` : ''));
    button.addEventListener('click', () => openExercise({ id: exercise.id }));
    row.append(button);
    list.append(row);
  }
  focus.append(list);
  if (!next.begin && state?.plans?.length) renderStartPicker(focus, state.plans);
}

function renderStartPicker(focus, plans) {
  const box = el('details', 'swap start-picker');
  box.append(el('summary', '', 'Start a workout now'));
  const actions = el('div', 'live-actions');
  for (const plan of plans) {
    const b = el('button', 'chip', `Start ${plan.name}`);
    b.type = 'button';
    b.addEventListener('click', () => send('/api/coach', { start_template: plan.id }, body => body?.ack || `Started ${plan.name}`));
    actions.append(b);
  }
  box.append(actions);
  focus.append(box);
}

/* A workout finished in the last 3 hours: its numbers, and "how busy was it?" until rated (the same
   buttons the chat's summary has). */
function renderFinished(focus, workout, next) {
  const heading = el('div', 'section-heading');
  heading.append(el('span', '', `Done · ${workout.label}`), el('span', '', next ? `Next ${next.template} ${next.when}` : ''));
  focus.append(heading, el('h2', 'title', `${workout.template} done`));
  const work = [`${workout.minutes} min`, workout.sets && `${workout.sets} sets`, workout.volume && tonnes(workout.volume)].filter(Boolean).join(' · ');
  focus.append(el('p', 'subtitle', work));
  if (workout.prs.length) focus.append(el('p', 'records', `New best: ${workout.prs.join(', ')}`));
  // The workout's last set can still be fixed here: a typo on the final set is common.
  if (workout.last && !logger.fixing) focus.append(lastLine(workout.last, null));
  if (workout.last && logger.fixing) renderLogger(focus, null, null, workout.last);
  if (workout.rate.length && !logger.fixing) {
    focus.append(el('h3', 'ask', 'How busy was the gym?'));
    const actions = el('div', 'live-actions');
    workout.rate.forEach(b => actions.append(actionButton(b)));
    focus.append(actions);
  }
}

/* Nothing planned: where the week stands, and the chat to plan more. */
function renderNothing(focus, data) {
  if (data.setup_done === false) {
    focus.append(el('h2', 'title', 'Finish setup in Telegram'));
    focus.append(el('p', 'subtitle', 'Answer a few quick questions in the chat so gymclaw can build your plan.'));
  } else if (data.insights) {
    const k = data.insights.kpis;
    focus.append(el('h2', 'title', 'Nothing planned'));
    focus.append(el('p', 'subtitle', `${k.this_week ?? 0} of ${data.insights.target} sessions this week${data.history?.[0] ? ` · last: ${data.history[0].template}, ${data.history[0].label}` : ''}`));
    focus.append(el('p', 'subtitle', 'Tell gymclaw in the chat when you want to train, and it plans the session.'));
  } else {
    focus.append(el('h2', 'title', 'Nothing planned'));
  }
  if (data.setup_done !== false && data.plans?.length) renderStartPicker(focus, data.plans);
  if (tg) {
    const chat = el('button', 'chip', 'Back to the chat');
    chat.type = 'button';
    chat.addEventListener('click', () => tg.close());
    focus.append(chat);
  }
}

function renderToday(data) {
  // A background refresh (the ETA ticks every minute) mustn't close the keypad mid-number: while a field
  // on the card has focus, the card waits and redraws when it's left.
  // Redrawing on blur would replace the button whose tap caused the blur, and the tap would be lost; the
  // next poll (3 s) or the action's own refresh draws it instead.
  if (document.getElementById('focus').contains(document.activeElement) && document.activeElement.tagName === 'INPUT') {
    rendered = '';
    return;
  }
  renderWeek(data.days, data.range);
  renderGym(data.crowd);
  renderActivity(data.activity, data.days[0]?.name);
  const focus = document.getElementById('focus');
  const prevQueue = focus.querySelector('.queue');
  const prevQueueScroll = prevQueue ? prevQueue.scrollLeft : null;
  focus.replaceChildren();
  clearInterval(restTimer);
  liveAction = null;
  const finished = !data.workout && data.history?.[0] && now() - Date.parse(data.history[0].finished_at) < 3 * 3600e3 ? data.history[0] : null;
  // Mid-workout a phone shows just the workout (styles.css); before a session its card is the dark one.
  document.getElementById('tab-today').classList.toggle('live', Boolean(data.workout));
  // A main button on the card (the set, or fixing the last one): the tab bar steps aside in Telegram.
  document.documentElement.classList.toggle('live-mode', Boolean(data.workout || (finished && logger.fixing)) && activeTab() === 'today');
  focus.classList.toggle('next', !data.workout && !finished && Boolean(data.next));
  if (data.workout) renderWorkout(focus, data.workout, prevQueueScroll);
  else {
    queueActiveId = null;
    if (finished) renderFinished(focus, finished, data.next);
    else if (data.next) renderNext(focus, data.next);
    else renderNothing(focus, data);
  }
  syncMain();
}

/* ---------- Progress ---------- */

function renderKpis(insights) {
  const k = insights.kpis;
  const change = k.volume_prev_7d ? Math.round((k.volume_7d - k.volume_prev_7d) / k.volume_prev_7d * 100) : null;
  const tiles = [
    ['Workouts', k.workouts_30d, 'last 30 days'],
    ['This week', `${k.this_week}/${insights.target}`, 'sessions done'],
    ['Volume', tonnes(k.volume_7d), change === null ? 'last 7 days' : `last 7 days · ${change >= 0 ? '+' : ''}${change}%`],
    ['Records', k.prs_30d, 'heavier top sets, 30 days'],
  ];
  const box = document.getElementById('kpis');
  box.replaceChildren();
  for (const [label, value, note] of tiles) {
    const tile = el('div', 'kpi');
    tile.append(el('span', '', label), el('strong', '', value), el('small', '', note));
    box.append(tile);
  }
}

function renderWeeks(insights) {
  const box = document.getElementById('weeks-chart');
  document.getElementById('target-label').textContent = `Target ${insights.target}`;
  const weeks = insights.weeks;
  // Even top so the midline is a whole number of sessions.
  const chart = frame(box, Math.ceil((Math.max(insights.target, ...weeks.map(w => w.count)) + 1) / 2) * 2, { label: 'Sessions per week' });
  const slot = (chart.W - chart.L - chart.R) / weeks.length;
  const width = Math.min(34, slot * 0.6);
  weeks.forEach((week, index) => {
    const cx = chart.L + slot * (index + 0.5);
    const top = chart.y(week.count), base = chart.y(0);
    if (week.count) {
      const height = base - top;
      const r = Math.min(4, height);
      chart.svg.append(svgEl('path', { class: `bar${week.count >= insights.target ? ' met' : ''}`,
        d: `M${cx - width / 2},${base} V${top + r} Q${cx - width / 2},${top} ${cx - width / 2 + r},${top} H${cx + width / 2 - r} Q${cx + width / 2},${top} ${cx + width / 2},${top + r} V${base} Z` }));
    }
    if (index % (weeks.length > 6 && chart.W < 420 ? 2 : 1) === 0) chart.svg.append(svgText(cx, chart.H - 6, week.label, { 'text-anchor': 'middle' }));
    const hit = svgEl('rect', { x: cx - slot / 2, y: chart.T, width: slot, height: chart.H - chart.T - chart.B, class: 'hit' });
    hover(hit, event => showTip(event, `Week of ${week.label}`, [[`${week.count} of ${insights.target}`, 'sessions']]));
    chart.svg.append(hit);
  });
  const target = chart.y(insights.target);
  chart.svg.insertBefore(svgEl('line', { x1: chart.L, x2: chart.W - chart.R, y1: target, y2: target, class: 'target-line' }), chart.svg.querySelector('.hit'));
}

function renderProgress(insights) {
  const select = document.getElementById('exercise-select');
  const box = document.getElementById('progress-chart');
  const series = insights.progress;
  if (!series.length) {
    select.hidden = true;
    box.replaceChildren(el('p', 'empty', 'Finish a workout to see progress here.'));
    return;
  }
  select.hidden = false;
  if (!series.some(s => s.id === selectedExercise)) {
    // Default: the exercise with the most sessions.
    selectedExercise = [...series].sort((a, b) => b.points.length - a.points.length)[0].id;
  }
  select.replaceChildren(...series.map(s => { const option = el('option', '', s.name); option.value = s.id; option.selected = s.id === selectedExercise; return option; }));
  drawProgress(box, series.find(s => s.id === selectedExercise).points);
}

/* Best set per session as a line: estimated 1RM, or reps for bodyweight. Progress and an exercise's
   detail both draw it; `box` must be visible, since the chart measures it. */
function drawProgress(box, points) {
  // Bodyweight exercises have no weight to estimate from: plot reps instead.
  const bodyweight = points.every(p => !p.weight);
  const value = p => bodyweight ? p.reps : toDisplay(p.e1rm);
  const values = points.map(value);
  const low = Math.max(0, Math.floor((Math.min(...values) * 0.9) / 5) * 5);
  const top = Math.ceil(Math.max(...values) * 1.05 / 5) * 5 || 5;
  const chart = frame(box, top - low, { label: bodyweight ? 'Best set reps per session' : 'Estimated 1-rep max per session' });
  // Shift labels: the axis starts at `low`, not 0, so small progress stays visible.
  chart.svg.querySelectorAll('text.label').forEach((label, i) => { label.textContent = Math.round(low + [0, (top - low) / 2, top - low][i]); });
  const y = v => chart.y(v - low);
  const xs = points.map((_, i) => points.length === 1 ? (chart.L + chart.W - chart.R) / 2 : chart.L + 12 + i * (chart.W - chart.L - chart.R - 24) / (points.length - 1));
  chart.svg.append(svgEl('path', { class: 'line', d: points.map((p, i) => `${i ? 'L' : 'M'}${xs[i]},${y(value(p))}`).join(' ') }));
  points.forEach((p, i) => chart.svg.append(svgEl('circle', { cx: xs[i], cy: y(value(p)), r: 4, class: 'point' })));
  const every = Math.ceil(points.length / (chart.W < 420 ? 4 : 7));
  points.forEach((p, i) => { if (i % every === 0 || i === points.length - 1) chart.svg.append(svgText(xs[i], chart.H - 6, p.label.replace(/^\w+ /, ''), { 'text-anchor': 'middle' })); });
  crosshair(chart, xs, i => ({ title: points[i].label, y: y(value(points[i])),
    rows: bodyweight ? [[`${points[i].reps} reps`, 'best set']] : [[`≈ ${kgs(points[i].e1rm)}`, '1RM'], [`${points[i].reps} × ${kgs(points[i].weight)}`, 'best set']] }));
}

function renderMuscles(insights) {
  const box = document.getElementById('muscle-chart');
  box.replaceChildren();
  if (!insights.muscles.length) {
    box.append(el('p', 'empty', 'No training in the last 4 weeks.'));
    return;
  }
  // Bodyweight-only weeks have no tonnage: never divide by zero.
  const max = Math.max(1, ...insights.muscles.map(m => m.volume));
  for (const muscle of insights.muscles) {
    const row = el('div', 'bar-row');
    const track = el('div', 'bar-track');
    const fill = el('div', 'bar-fill');
    fill.style.width = `${Math.max(2, muscle.volume / max * 100)}%`;
    track.append(fill);
    row.append(el('span', 'bar-label', muscle.muscle), track, el('span', 'bar-value', tonnes(muscle.volume)));
    hover(row, event => showTip(event, muscle.muscle, [[tonnes(muscle.volume), 'volume'], [muscle.sets, 'working sets']]));
    box.append(row);
  }
}

function renderHeatmap(insights) {
  const box = document.getElementById('heatmap');
  const panel = box.closest('section');
  box.replaceChildren();
  const cells = insights.heatmap;
  const label = document.getElementById('quiet-label');
  if (!cells.length && !state?.crowd?.polling) {
    if (panel) panel.hidden = true;
    return;
  }
  if (panel) panel.hidden = false;
  if (!cells.length) {
    label.textContent = '';
    box.append(el('p', 'empty', 'Collecting check-ins…'));
    return;
  }
  const hours = cells.map(c => c.hour);
  const first = Math.min(...hours), last = Math.max(...hours);
  const max = Math.max(...cells.map(c => c.count));
  const lookup = new Map(cells.map(c => [`${c.weekday}-${c.hour}`, c]));
  // Quietest daytime hour with data (night hours at a 24/7 gym aren't useful advice).
  const daytime = cells.filter(c => c.hour >= 7 && c.hour <= 21);
  const quiet = (daytime.length ? daytime : cells).reduce((a, b) => (b.count < a.count ? b : a));
  label.textContent = `Quietest: ${WEEKDAYS[quiet.weekday]} ${clock(quiet.hour * 60)}`;
  box.style.setProperty('--hours', last - first + 1);
  box.append(el('span'));
  for (let h = first; h <= last; h++) box.append(el('span', 'hm-hour', h % 3 === 0 || h === first ? String(h).padStart(2, '0') : ''));
  WEEKDAYS.forEach((name, day) => {
    box.append(el('span', 'hm-day', name));
    for (let h = first; h <= last; h++) {
      const cell = lookup.get(`${day}-${h}`);
      const node = el('span', `hm-cell${cell ? '' : ' none'}`);
      if (cell) {
        // One hue, light to dark: busier is darker.
        node.style.setProperty('--t', (cell.count / max).toFixed(3));
        hover(node, event => showTip(event, `${name} ${clock(h * 60)}–${clock(h * 60 + 60)}`, [[`~${cell.count}`, 'checked in'], [cell.days, cell.days === 1 ? 'day of data' : 'days of data']]));
      }
      box.append(node);
    }
  });
}

/* Daily weight swings 1–2 kg: raw weigh-ins as faint dots, the 7-day average as the line. Time-scaled x,
   because weigh-ins are irregular (a backfill may be weekly, then daily). */
function renderWeight(insights) {
  const box = document.getElementById('weight-chart');
  const points = insights.weight || [];
  document.getElementById('weight-legend').hidden = !points.length;
  if (!points.length) {
    box.replaceChildren(el('p', 'empty', 'Send gymclaw a photo of your scale to start tracking.'));
    renderWeightLog([]);
    return;
  }
  const values = points.flatMap(p => [toDisplayBody(p.kg), toDisplayBody(p.avg)]);
  const low = Math.floor(Math.min(...values) - 0.5), top = Math.ceil(Math.max(...values) + 0.5);
  const chart = frame(box, top - low, { label: 'Body weight: weigh-ins and 7-day average' });
  chart.svg.querySelectorAll('text.label').forEach((label, i) => { label.textContent = low + [0, (top - low) / 2, top - low][i]; });
  const day = date => Date.parse(date) / 864e5;
  const first = day(points[0].date), span = Math.max(1, day(points.at(-1).date) - first);
  const x = date => chart.L + 12 + (day(date) - first) / span * (chart.W - chart.L - chart.R - 24);
  const y = kg => chart.y(kg - low);
  const ticks = Math.min(span, chart.W < 420 ? 3 : 6);
  for (let i = 0; i <= ticks; i++) {
    const at = new Date((first + span * i / ticks) * 864e5);
    chart.svg.append(svgText(x(at.toISOString().slice(0, 10)), chart.H - 6, `${at.getUTCDate()} ${at.toLocaleDateString('en-US', { month: 'short', timeZone: 'UTC' })}`, { 'text-anchor': 'middle' }));
  }
  points.forEach(p => chart.svg.append(svgEl('circle', { cx: x(p.date), cy: y(toDisplayBody(p.kg)), r: 3, class: 'weigh-in' })));
  chart.svg.append(svgEl('path', { class: 'line', d: points.map((p, i) => `${i ? 'L' : 'M'}${x(p.date).toFixed(1)},${y(toDisplayBody(p.avg)).toFixed(1)}`).join(' ') }));
  crosshair(chart, points.map(p => x(p.date)), i => ({ title: points[i].label,
    rows: [[bodyWeightText(points[i].kg), 'weigh-in', 'weigh-in'], [bodyWeightText(points[i].avg), '7-day average', 'today']], y: y(toDisplayBody(points[i].avg)) }));
  renderWeightLog(points);
}

function renderWeightLog(points) {
  const host = document.getElementById('weight-log');
  if (!host || host.contains(document.activeElement)) return;
  host.replaceChildren();
  const last = points.at(-1)?.kg;
  let value = last !== undefined ? toDisplayBody(last) : null;
  host.append(stepper('Body weight', value, { step: 0.1, min: isLb() ? 45 : 20, base: isLb() ? 165 : 75, unit: weightUnit(), decimal: true, onChange: v => { value = v; } }));
  const save = el('button', 'save', 'Log');
  save.type = 'button';
  save.addEventListener('click', async () => {
    if (!value) { toast('Enter a weight first', true); return; }
    save.disabled = true;
    try {
      await toolCall('body_log', { entries: [{ kg: toKg(value) }] });
      haptic.done();
      toast(`Logged ${value} ${weightUnit()}`);
    } catch (error) {
      haptic.done(false);
      toast(error.message, true);
    } finally { save.disabled = false; }
  });
  host.append(save);
}

function renderInsights(data) {
  renderKpis(data.insights);
  renderWeeks(data.insights);
  renderProgress(data.insights);
  renderMuscles(data.insights);
  renderHeatmap(data.insights);
  renderWeight(data.insights);
}

/* ---------- History ---------- */


function renderHistory(history) {
  const box = document.getElementById('history');
  // Keep expanded workouts expanded when new data re-renders the list.
  const open = new Set([...box.querySelectorAll('details[open]')].map(item => item.dataset.key));
  box.replaceChildren();
  document.getElementById('history-count').textContent = `${history.length} workout${history.length === 1 ? '' : 's'}`;
  if (!history.length) {
    box.append(el('p', 'empty', 'No finished workouts yet.'));
    return;
  }
  for (const workout of history) {
    const item = el('details', 'workout');
    item.dataset.key = `${workout.date} ${workout.time}`;
    item.open = open.has(item.dataset.key);
    const summary = el('summary');
    const when = el('div', 'when');
    when.append(el('strong', '', workout.label), el('span', '', workout.time));
    const what = el('div', 'what');
    // A ride or a plank shows its time, not "0 sets · 0 kg".
    const timed = workout.exercises.flatMap(e => e.sets).reduce((sum, s) => sum + (s.seconds || 0), 0);
    const work = [timed && `${duration(timed)} timed`, workout.sets && `${workout.sets} sets`, workout.volume && tonnes(workout.volume)];
    what.append(el('strong', '', workout.template), el('span', '', [`${workout.minutes} min`, ...work.filter(Boolean)].join(' · ')));
    const tags = el('div', 'tags');
    if (workout.prs.length) tags.append(el('em', 'tag pr', `${workout.prs.length} PR${workout.prs.length > 1 ? 's' : ''}`));
    if (workout.crowd) tags.append(el('em', `tag ${dataClass('crowd', workout.crowd)}`, workout.crowd));
    summary.append(when, what, tags);
    const table = el('div', 'sets');
    for (const exercise of workout.exercises) {
      const row = el('div', 'set-row');
      // The name opens the exercise, as everywhere else in the app.
      const name = el('button', 'set-name', exercise.name);
      name.type = 'button';
      name.addEventListener('click', () => openExercise({ id: exercise.id, name: exercise.name, svg_url: exercise.svg_url }));
      if (workout.prs.includes(exercise.name)) name.append(el('em', 'tag pr', 'PR'));
      const list = el('span', 'set-list');
      for (const s of exercise.sets) {
        const chip = el('button', 'set-chip', s.text || (s.seconds ? duration(s.seconds) : `${s.reps}×${s.weight}`));
        chip.type = 'button';
        chip.title = 'Tap to edit set';
        chip.addEventListener('click', () => openSetEditor(exercise, s));
        list.append(chip);
      }
      row.append(name, list);
      table.append(row);
    }
    item.append(summary, table);
    box.append(item);
  }
}

/* ---------- Memory ---------- */

const PLACEHOLDER = { owner: 'Add something gymclaw should know', learned: 'Add a pattern' };

/* Runs one gymclaw tool (the server allows a few), then reloads the state; true if that reload
   worked. A refused call throws with the domain's message, e.g. why a plan edit is invalid. */
async function toolCall(tool, args) {
  const response = await fetch(`/api/${tool}`, { method: 'POST', headers: apiHeaders({ 'Content-Type': 'application/json' }), body: JSON.stringify(args) });
  if (!response.ok) {
    const body = await response.json().catch(() => null);
    // The first line is the reason; the rest is retry advice for the agent.
    throw new Error(body?.error?.message?.split('\n')[0] || `${tool} failed`);
  }
  return refresh();
}

/* A Memory change that didn't go through: the row is marked, and it's said and felt. */
function failed(node, error) {
  node.classList.add('failed');
  haptic.done(false);
  toast(`Didn't save: ${error.message}`, true);
}

/* One line per note: click to edit in place, Enter saves, Esc cancels. An emptied line reverts. */
function editable(text, placeholder, save) {
  const node = el('span', 'note-text', text);
  node.contentEditable = 'plaintext-only';
  node.spellcheck = true;
  node.enterKeyHint = 'done';
  node.dataset.placeholder = placeholder;
  node.addEventListener('keydown', event => {
    if (event.key === 'Enter') { event.preventDefault(); node.blur(); }
    if (event.key === 'Escape') { node.textContent = text; node.blur(); }
  });
  node.addEventListener('blur', async () => {
    const next = node.textContent.trim();
    if (!next || next === text) { node.textContent = text; return; }
    // A failed save keeps what was typed, so it can be tried again.
    try { await save(next); haptic.done(); } catch (error) { node.closest('li').classList.add('failed'); haptic.done(false); toast(`Not saved: ${error.message}`, true); }
  });
  return node;
}

function renderMemory(memory) {
  // Memory has a character budget per kind; past it the agent condenses before adding.
  for (const usage of document.querySelectorAll('.usage')) {
    const { used, limit } = memory.usage[usage.dataset.kind];
    // Only worth a glance once it's filling up.
    usage.textContent = used >= limit * 0.8 ? ` · ${Math.round(used / limit * 100)}% full` : '';
    usage.classList.toggle('warn', used >= limit * 0.9);
  }
  for (const list of document.querySelectorAll('.notes[data-kind]')) {
    // Don't replace a line while it's being typed in; the next refresh catches up.
    if (list.contains(document.activeElement)) continue;
    const kind = list.dataset.kind;
    list.replaceChildren();
    for (const note of memory[kind]) {
      const item = el('li', 'note');
      const forget = el('button', 'forget', '×');
      forget.setAttribute('aria-label', 'Forget');
      forget.addEventListener('click', () => {
        if (tgHas('6.2')) {
          tg.showConfirm(`Forget “${note.text}”?`, ok => ok && toolCall('note_delete', { id: note.id }).catch(error => failed(item, error)));
          return;
        }
        // Two taps: the first arms it, the second forgets.
        if (!item.classList.contains('armed')) { item.classList.add('armed'); forget.textContent = 'Forget?'; return; }
        forget.blur();
        toolCall('note_delete', { id: note.id }).catch(error => failed(item, error));
      });
      forget.addEventListener('blur', () => { item.classList.remove('armed'); forget.textContent = '×'; });
      const text = editable(note.text, '', next => toolCall('note_edit', { id: note.id, text: next }));
      item.append(text, el('time', '', ago(note.updated_at)), forget);
      list.append(item);
    }
    const add = el('li', 'note add');
    add.append(editable('', PLACEHOLDER[kind], next => toolCall('note_add', { kind, text: next })));
    list.append(add);
  }
}

/* What the agent built: one line each with an on/off switch, its versions (restore any) under it.
   Then the requests it couldn't handle, newest first. Both edit only through extension_edit. */
function renderExtensions(extensions) {
  const list = document.getElementById('extensions');
  list.replaceChildren();
  for (const item of extensions.items) {
    const row = el('li', `note ext${item.enabled ? '' : ' off'}`);
    const details = el('details', 'note-text');
    const summary = el('summary');
    summary.append(el('strong', '', item.name), ` · ${item.kind} · ${item.summary}`);
    details.append(summary);
    for (const v of item.versions) {
      const line = el('div', 'ext-version', `v${v.version} ${v.when} · ${v.author}${v.enabled ? '' : ' · off'} · ${v.summary}`);
      if (!v.same) {
        const restore = el('button', 'forget', 'Restore');
        restore.addEventListener('click', () => toolCall('extension_edit', { id: item.id, restore_version: v.version }).catch(error => failed(row, error)));
        line.append(' ', restore);
      }
      details.append(line);
    }
    const toggle = el('button', 'forget', item.enabled ? 'Turn off' : 'Turn on');
    toggle.addEventListener('click', () => toolCall('extension_edit', { id: item.id, enabled: !item.enabled }).catch(error => failed(row, error)));
    row.append(details, toggle);
    list.append(row);
  }
  if (!extensions.items.length) list.append(el('li', 'note empty', 'Nothing yet.'));
  const gaps = document.getElementById('gaps');
  gaps.replaceChildren();
  for (const gap of extensions.gaps) {
    const row = el('li', 'note');
    row.append(el('span', 'note-text', `${gap.asked} — ${gap.why}${gap.source === 'agent' ? ` (tried: ${gap.tried})` : ''}`), el('time', '', gap.when));
    gaps.append(row);
  }
  gaps.closest('section').hidden = !extensions.gaps.length;
}

/* ---------- Exercises ---------- */

// Head to toe, so lists read like a body map; anything else sorts last.
const MUSCLES = ['Chest', 'Shoulders', 'Rear Delts', 'Triceps', 'Biceps', 'Forearms', 'Back', 'Lats', 'Upper Back', 'Lower Back', 'Core',
  'Glutes', 'Quads', 'Hamstrings', 'Legs', 'Adductors', 'Hips', 'Calves', 'Posterior Chain', 'Mobility'];
// Catalog chips: the common kinds; the rest (wall, towel, chair, …) share "Other".
const EQUIPMENT = ['Machine', 'Cable', 'Dumbbell', 'Barbell', 'Bodyweight', 'Pull-up Bar', 'Resistance Band', 'Cardio'];
const exView = { view: 'plans', query: '', equipment: null };
let catalog = null;
let detailOf = null;

// A timed set's length: '25 min', '45 s', '1 min 30 s'.
const duration = seconds => [[Math.floor(seconds / 3600), 'h'], [Math.floor(seconds % 3600 / 60), 'min'], [seconds % 60, 's']]
  .filter(([n]) => n).map(([n, unit]) => `${n} ${unit}`).join(' ') || '0 s';
const setText = set => set.text ? set.text : set.seconds ? duration(set.seconds) : set.weight ? `${set.reps} × ${kgs(set.weight)}` : `${set.reps} reps`;
// Heaviest set; more reps break a tie. Timed: the longest.
/* Heaviest set; `rank` is the weight with better up (less help on an assisted machine). */
const rank = s => s.rank ?? s.weight;
const bestSet = sets => sets.reduce((a, b) => (rank(b) > rank(a) || (rank(b) === rank(a) && (b.reps > a.reps || (b.seconds || 0) > (a.seconds || 0))) ? b : a));
// What "best" means for the set: the longest plank, the farthest row, the most reps, the heaviest lift.
const bestNote = set => set.seconds ? 'longest set' : set.meters ? 'farthest set' : !set.weight ? 'most reps' : 'heaviest set';
const muscleRank = muscle => { const i = MUSCLES.indexOf(muscle); return i < 0 ? MUSCLES.length : i; };

/* Every exercise ever logged, from History (newest workout first), with its sessions newest first. */
function loggedExercises() {
  const byId = new Map();
  for (const workout of state?.history || []) for (const exercise of workout.exercises) {
    if (!exercise.sets.length) continue;
    const entry = byId.get(exercise.id) || { id: exercise.id, name: exercise.name, muscle: exercise.muscle, svg_url: exercise.svg_url, sessions: [] };
    entry.sessions.push({ label: workout.label, template: workout.template, sets: exercise.sets, pr: workout.prs.includes(exercise.name) });
    byId.set(exercise.id, entry);
  }
  return byId;
}

/* Items grouped by muscle, head to toe: [[muscle, items], …]. */
function byMuscle(items) {
  const groups = new Map();
  for (const item of items) groups.set(item.muscle || 'Other', [...(groups.get(item.muscle || 'Other') || []), item]);
  return [...groups.entries()].sort((a, b) => muscleRank(a[0]) - muscleRank(b[0]) || a[0].localeCompare(b[0]));
}

function exerciseRow(item, sub, aside, open) {
  const row = el('button', 'ex-row');
  row.type = 'button';
  const text = el('div');
  text.append(el('strong', '', item.name), el('span', '', sub));
  row.append(thumb(item.svg_url), text, el('em', '', aside || ''));
  row.addEventListener('click', open);
  return row;
}

function group(title, note, children) {
  const panel = el('section', 'panel ex-group');
  const heading = el('div', 'section-heading');
  heading.append(el('h2', '', title), el('span', '', note));
  panel.append(heading, ...children);
  return panel;
}

async function loadCatalog() {
  if (catalog) return;
  try {
    const response = await fetch('/api/catalog', { headers: apiHeaders() });
    if (!response.ok) throw new Error('catalog');
    catalog = await response.json();
  } catch { catalog = []; }
}

function renderExercises() {
  document.querySelectorAll('#ex-views button').forEach(button => button.setAttribute('aria-pressed', String(button.dataset.ex === exView.view)));
  const list = document.getElementById('ex-list');
  list.replaceChildren();
  const query = exView.query.trim().toLowerCase();
  const matches = (...texts) => !query || texts.some(text => String(text || '').toLowerCase().includes(query));
  const logged = loggedExercises();
  const chips = document.getElementById('ex-chips');
  chips.hidden = exView.view !== 'catalog';
  if (exView.view === 'plans') {
    for (const plan of state.plans) {
      const exercises = plan.exercises.filter(e => matches(e.name, e.muscle, plan.name));
      if (!exercises.length) continue;
      list.append(group(plan.name, `${exercises.length} exercise${exercises.length === 1 ? '' : 's'}`, exercises.map(e => {
        const last = logged.get(e.id)?.sessions[0];
        const target = e.sets > 1 || (e.measure || 'reps') === 'reps' ? `${e.sets} × ${e.target}` : e.target;
        return exerciseRow(e, target, last ? `Last ${setText(bestSet(last.sets))}` : 'Not logged yet', () => openExercise({ id: e.id }));
      })));
    }
  } else if (exView.view === 'done') {
    const done = [...logged.values()].filter(e => matches(e.name, e.muscle));
    for (const [muscle, items] of byMuscle(done)) {
      list.append(group(muscle, `${items.length}`, items.map(e => {
        const sessions = `${e.sessions.length} session${e.sessions.length === 1 ? '' : 's'} · last ${e.sessions[0].label}`;
        return exerciseRow(e, sessions, setText(bestSet(e.sessions[0].sets)), () => openExercise({ id: e.id }));
      })));
    }
  } else {
    if (!catalog) { loadCatalog().then(() => exView.view === 'catalog' && renderExercises()); list.append(el('p', 'empty', 'Loading…')); return; }
    const kind = equipment => EQUIPMENT.includes(equipment) ? equipment : 'Other';
    chips.replaceChildren(...[null, ...EQUIPMENT, 'Other'].map(value => {
      const chip = el('button', 'chip', value || 'All');
      chip.type = 'button';
      chip.setAttribute('aria-pressed', String(exView.equipment === value));
      chip.addEventListener('click', () => { haptic.select(); exView.equipment = value; renderExercises(); });
      return chip;
    }));
    const yours = new Set(state.plans.flatMap(p => p.exercises.map(e => e.guide_id)));
    const items = catalog.filter(i => matches(i.name, i.muscle, i.equipment) && (!exView.equipment || kind(i.equipment) === exView.equipment));
    for (const [muscle, entries] of byMuscle(items)) {
      const tiles = el('div', 'ex-tiles');
      for (const item of entries) {
        const tile = el('button', `ex-tile${yours.has(item.guide_id) ? ' yours' : ''}`);
        tile.type = 'button';
        tile.append(thumb(item.svg_url), el('span', '', item.name), el('small', '', item.equipment));
        if (yours.has(item.guide_id)) tile.append(el('em', 'tag', 'In a plan'));
        tile.addEventListener('click', () => openExercise({ guide: item }));
        tiles.append(tile);
      }
      list.append(group(muscle, `${entries.length}`, [tiles]));
    }
  }
  if (!list.children.length) list.append(el('p', 'empty', query ? `Nothing matches “${exView.query.trim()}”.` : 'Nothing here yet.'));
}

/* Plan targets of one exercise in one plan. Saves through template_edit_exercise by name: names are
   unique within a plan, while an id can also be another variant's picture. */
function planEditor(plan, exercise) {
  const form = el('form', 'ex-edit');
  form.dataset.plan = plan.id;
  // Mid-workout this edits the plan, not the sets on screen: say so.
  form.append(el('h3', '', `Plan · ${plan.name}${state.workout ? ' · from next session' : ''}`));
  // Names are ExerciseSpec fields, sent as they are.
  // Empty rest means the profile's default for the exercise's role; it shows blank until changed.
  // What the plan can set depends on how the exercise is done: a timed or distance one has no reps or
  // weight here; weight shows when it's lifted, added (dip belt) or assisted.
  const reps = (exercise.measure || 'reps') === 'reps';
  const fields = [['working_sets', 'Sets', exercise.sets, 1, '']];
  if (reps) fields.push(['rep_min', 'Reps from', exercise.rep_min, 1, 'reps'], ['rep_max', 'Reps to', exercise.rep_max, 1, 'reps']);
  fields.push(['rest_seconds', 'Rest', exercise.rest_seconds, 15, 's']);
  const load = exercise.load || 'external';
  if (reps && (!exercise.bodyweight || load !== 'external')) {
    fields.unshift(['target_weight', load === 'added' ? 'Added' : 'Weight', toDisplay(exercise.weight), weightStep(exercise.equipment, exercise.weight), load === 'assisted' ? `${weightUnit()} assist` : weightUnit()]);
  }
  const was = Object.fromEntries(fields.map(([name, , value]) => [name, value ?? null]));
  const values = { ...was };
  const grid = el('div', 'ex-fields');
  for (const [name, label, value, step, unit] of fields) {
    grid.append(stepper(label, value, { step, unit, min: name === 'target_weight' ? 0 : name === 'rest_seconds' ? 15 : 1, nullable: name === 'rest_seconds', decimal: name === 'target_weight',
      onChange: v => { values[name] = v; markDirty(form, `Save ${plan.name}`); } }));
  }
  const save = el('button', 'save', 'Save');
  save.type = 'submit';
  const message = el('span', 'ex-message');
  const actions = el('div', 'ex-actions');
  actions.append(save, message);
  form.append(grid, actions);
  form.addEventListener('submit', async event => {
    event.preventDefault();
    const changes = Object.fromEntries(Object.entries(values).filter(([name, value]) => value !== was[name] && (value !== null || name === 'rest_seconds')).map(([name, value]) => [name, name === 'target_weight' ? toKg(value) : value]));
    if (!Object.keys(changes).length) { message.textContent = 'Nothing changed'; clearDirty(); return; }
    save.disabled = true;
    message.textContent = 'Saving…';
    tg?.MainButton.showProgress();
    try {
      const reloaded = await toolCall('template_edit_exercise', { template_id: plan.id, exercise: exercise.name, changes });
      clearDirty();
      haptic.done();
      if (!reloaded) {
        message.textContent = 'Saved · reload to see it';
        return;
      }
      openExercise(detailOf);
      const saved = document.querySelector(`.ex-edit[data-plan="${CSS.escape(plan.id)}"] .ex-message`);
      if (saved) saved.textContent = 'Saved';
    } catch (error) {
      message.textContent = error.message;
      save.disabled = false;
      tg?.MainButton.hideProgress();
      haptic.done(false);
    }
  });
  return form;
}

/* Your exercises drawn with a catalog entry's picture: plan exercises, then ones only in your history.
   History rows know their picture, not their guide_id, so the picture URL is the link. */
function exercisesWithPicture(guide) {
  const yours = (state?.plans || []).flatMap(plan => plan.exercises.filter(e => e.guide_id === guide.guide_id).map(e => ({ e, where: plan.name })));
  for (const e of loggedExercises().values()) {
    if (e.svg_url === guide.svg_url && !yours.some(y => y.e.id === e.id)) yours.push({ e, where: 'Not in a plan' });
  }
  return yours;
}

/* Detail of an exercise you train or logged ({id}), or of a catalog entry ({guide}). */
function openExercise(of) {
  if (of.guide) {
    const yours = exercisesWithPicture(of.guide);
    // One exercise of yours uses this picture: its detail says more than the catalog entry.
    if (yours.length === 1) return openExercise({ id: yours[0].e.id });
  }
  detailOf = of;
  const dialog = document.getElementById('ex-detail');
  // The sheet holds the content, so a tap on the dialog itself is a tap on the backdrop.
  const sheet = el('div', 'ex-sheet');
  dialog.replaceChildren(sheet);
  const close = el('button', 'ex-close', '×');
  close.type = 'button';
  close.setAttribute('aria-label', 'Close');
  close.addEventListener('click', closeSheet);
  const occurrences = of.id ? (state?.plans || []).flatMap(plan => plan.exercises.filter(e => e.id === of.id).map(exercise => ({ plan, exercise }))) : [];
  const logged = of.id ? loggedExercises().get(of.id) : null;
  // An exercise swapped in mid-workout may be in no plan and have no history yet: its queue tile names it.
  const item = of.guide || occurrences[0]?.exercise || logged || (of.name && { name: of.name, svg_url: of.svg_url });
  if (!item) return;
  const head = el('div', 'ex-head');
  const pose = el('div', 'ex-pose');
  pose.append(thumb(item.svg_url));
  const titles = el('div');
  titles.append(el('h2', 'title', item.name), el('p', 'subtitle', [item.muscle, item.equipment || of.guide?.equipment].filter(Boolean).join(' · ')));
  titles.querySelector('h2').id = 'ex-detail-title';
  head.append(pose, titles);
  sheet.append(close, head);
  if (of.now) {
    // Mid-workout, still to do: switch to it right away (the chat's card follows).
    // Its own button, also in Telegram: the sheet's main button is for saving plan edits.
    const now = el('button', 'save log-button in-sheet', `Do ${item.name} now`);
    now.type = 'button';
    now.addEventListener('click', () => { dialog.close(); coachTap(of.now); });
    sheet.append(now);
    // Switching ends a half-done exercise where it is (its sets stay logged): say so before the tap.
    const current = state.workout?.exercises.find(e => e.id === state.workout.active?.id);
    if (current && current.done > 0 && current.done < current.sets) sheet.append(el('p', 'subtitle', `${current.name} ends at ${current.done} of ${current.sets} sets.`));
  }
  if (of.guide) {
    const yours = exercisesWithPicture(of.guide);
    if (yours.length) {
      sheet.append(el('h3', '', 'Yours with this picture'));
      for (const { e, where } of yours) sheet.append(exerciseRow(e, where, '', () => openExercise({ id: e.id })));
    } else sheet.append(el('p', 'empty', 'You haven\'t done this one yet.'));
  }
  if (!of.guide && !logged && !occurrences.length) sheet.append(el('p', 'empty', 'No history for this exercise yet.'));
  if (logged) {
    const all = logged.sessions.flatMap(s => s.sets);
    const stats = el('div', 'ex-stats');
    for (const [label, value, note] of [['Latest', setText(bestSet(logged.sessions[0].sets)), logged.sessions[0].label],
      ['Best', setText(bestSet(all)), bestNote(bestSet(all))], ['Sessions', logged.sessions.length, `${all.length} sets`]]) {
      const stat = el('div', 'kpi');
      // "10 × 86.5 kg": the unit goes small so the number fits on one line.
      const units = / (kg assist|lb assist|kg|lb|reps|min|km|m|s)$/;
      const strong = el('strong', '', String(value).replace(units, ''));
      const unit = String(value).match(units);
      if (unit) strong.append(el('small', '', unit[1]));
      stat.append(el('span', '', label), strong, el('small', '', note));
      stats.append(stat);
    }
    sheet.append(stats);
  }
  for (const { plan, exercise } of occurrences) sheet.append(planEditor(plan, exercise));
  const points = of.id ? (state?.insights?.progress || []).find(s => s.id === of.id)?.points || [] : [];
  const chart = el('div', 'chart');
  if (points.length > 1) sheet.append(el('h3', '', points.every(p => !p.weight) ? 'Best set reps' : 'Estimated 1RM'), chart);
  if (logged) {
    sheet.append(el('h3', '', 'History'));
    const sessions = el('div', 'ex-history');
    for (const session of logged.sessions) {
      const row = el('div', 'ex-session');
      const when = el('span', 'when-cell', session.label);
      if (session.pr) when.append(el('em', 'tag pr', 'PR'));
      row.append(when, el('span', 'set-list', session.sets.map(setText).join('   ')));
      sessions.append(row);
    }
    sheet.append(sessions);
  }
  if (!dialog.open) dialog.showModal();
  syncBack();
  // The sheet has its own main action (saving a plan edit): the live set's "Log" isn't armed underneath.
  clearDirty();
  if (points.length > 1) drawProgress(chart, points);
}

/* Edit any logged set's numbers (never delete/retype) via set_edit. */
function openSetEditor(exercise, set) {
  const dialog = document.getElementById('ex-detail');
  const sheet = el('div', 'ex-sheet');
  dialog.replaceChildren(sheet);
  const close = el('button', 'ex-close', '×');
  close.type = 'button';
  close.setAttribute('aria-label', 'Close');
  close.addEventListener('click', closeSheet);
  const title = el('h2', 'title', `Edit ${exercise.name}`);
  title.id = 'ex-detail-title';
  sheet.append(close, title, el('p', 'subtitle', `Currently ${set.text}`));
  const form = el('form', 'ex-edit');
  const grid = el('div', 'ex-fields');
  const draft = { weight: toDisplay(set.weight), reps: set.reps, seconds: set.seconds, meters: set.meters };
  if (set.seconds) {
    const long = set.seconds >= 120;
    grid.append(stepper(long ? 'Time' : 'Seconds', long ? set.seconds / 60 : set.seconds,
      { step: long ? 1 : 5, min: long ? 1 : 5, unit: long ? 'min' : 's', decimal: long,
        onChange: v => { draft.seconds = v === null ? null : Math.round(long ? v * 60 : v); markDirty(form, 'Save set'); } }));
  } else if (set.meters) {
    const km = set.meters >= 1000;
    grid.append(stepper('Distance', km ? set.meters / 1000 : set.meters,
      { step: km ? 0.1 : 50, min: km ? 0.1 : 50, unit: km ? 'km' : 'm', decimal: km,
        onChange: v => { draft.meters = v === null ? null : Math.round(km ? v * 1000 : v); markDirty(form, 'Save set'); } }));
  } else {
    if (set.weight || exercise.added) {
      grid.append(stepper(exercise.added ? 'Added' : 'Weight', draft.weight,
        { step: weightStep(exercise.equipment, set.weight), min: 0, unit: exercise.unit || weightUnit(), decimal: true,
          onChange: v => { draft.weight = v; markDirty(form, 'Save set'); } }));
    }
    grid.append(stepper(exercise.per_side ? 'Reps / side' : 'Reps', draft.reps,
      { step: 1, min: 1, unit: 'reps', onChange: v => { draft.reps = v; markDirty(form, 'Save set'); } }));
  }
  const save = el('button', 'save log-button', 'Save set');
  save.type = 'submit';
  const actions = el('div', 'ex-actions');
  actions.append(save);
  form.append(grid, actions);
  form.addEventListener('submit', async event => {
    event.preventDefault();
    const payload = { set_id: set.id };
    if (set.seconds) {
      if (!draft.seconds) { toast('Enter a valid time', true); return; }
      payload.minutes = draft.seconds / 60;
    } else if (set.meters) {
      if (!draft.meters) { toast('Enter a valid distance', true); return; }
      payload.meters = draft.meters;
    } else {
      if (!draft.reps || ((set.weight || exercise.added) && draft.weight === null)) {
        toast('Enter valid numbers first', true);
        return;
      }
      payload.weight = toKg(draft.weight || 0);
      payload.reps = draft.reps;
    }
    save.disabled = true;
    try {
      await toolCall('set_edit', payload);
      clearDirty();
      haptic.done();
      dialog.close();
      toast('Set updated');
    } catch (error) {
      save.disabled = false;
      haptic.done(false);
      toast(error.message, true);
    }
  });
  sheet.append(form);
  if (!dialog.open) dialog.showModal();
  syncBack();
  clearDirty();
}

/* Edit profile timezone in a sheet with device-zone shortcut and IANA autocomplete. */
function openTimezoneEditor(current, browserTz) {
  const dialog = document.getElementById('ex-detail');
  const sheet = el('div', 'ex-sheet');
  dialog.replaceChildren(sheet);
  const close = el('button', 'ex-close', '×');
  close.type = 'button';
  close.setAttribute('aria-label', 'Close');
  close.addEventListener('click', closeSheet);
  const title = el('h2', 'title', 'Timezone');
  title.id = 'ex-detail-title';
  sheet.append(close, title, el('p', 'subtitle', `Currently ${current}`));

  const form = el('form', 'ex-edit');
  const input = document.createElement('input');
  input.type = 'text';
  input.className = 'tz-input';
  input.value = current;
  input.placeholder = 'e.g. America/New_York';
  input.autocomplete = 'off';
  input.autocapitalize = 'none';
  input.spellcheck = false;
  if (typeof Intl.supportedValuesOf === 'function') {
    const listId = 'tz-options';
    const list = document.createElement('datalist');
    list.id = listId;
    for (const tz of Intl.supportedValuesOf('timeZone')) {
      const opt = document.createElement('option');
      opt.value = tz;
      list.append(opt);
    }
    input.setAttribute('list', listId);
    form.append(list);
  }
  form.append(input);

  const actions = el('div', 'ex-actions');
  if (browserTz && browserTz !== current) {
    const useDevice = el('button', 'chip sm', `Use ${browserTz}`);
    useDevice.type = 'button';
    useDevice.addEventListener('click', () => { input.value = browserTz; });
    actions.append(useDevice);
  }
  const save = el('button', 'save log-button in-sheet', 'Save timezone');
  save.type = 'submit';
  actions.append(save);
  form.append(actions);

  form.addEventListener('submit', async event => {
    event.preventDefault();
    const tz = input.value.trim();
    if (!tz) { toast('Enter an IANA timezone', true); return; }
    save.disabled = true;
    try {
      await toolCall('profile_update', { changes: { timezone: tz } });
      haptic.done();
      dialog.close();
      toast(`Timezone set to ${tz}`);
    } catch (error) {
      save.disabled = false;
      haptic.done(false);
      toast(error.message, true);
    }
  });
  sheet.append(form);
  if (!dialog.open) dialog.showModal();
  syncBack();
  clearDirty();
}

/* Settings panel on the rebranded Settings + Memory tab. */
let restSaveTimer = null;
let restSaving = false;
function renderSettings(data) {
  const host = document.getElementById('settings');
  if (!host || !data.profile || restSaveTimer || restSaving) return;
  host.replaceChildren();
  const on = Boolean(data.runtime?.enabled);
  const rows = el('div', 'settings-rows');

  const row = (title, sub, control) => {
    const r = el('div', 'setting-row');
    const meta = el('div', 'setting-meta');
    meta.append(el('span', 'setting-label', title));
    if (sub) meta.append(el('small', '', sub));
    const ctrl = el('div', 'setting-control');
    ctrl.append(control);
    r.append(meta, ctrl);
    return r;
  };

  // 1. Weight unit
  const unitSeg = el('div', 'segmented');
  for (const u of ['kg', 'lb']) {
    const b = el('button', '', u);
    b.type = 'button';
    b.setAttribute('aria-pressed', String(data.profile.weight_unit === u));
    b.addEventListener('click', async () => {
      if (data.profile.weight_unit === u) return;
      haptic.select();
      try { await toolCall('profile_update', { changes: { weight_unit: u } }); haptic.done(); toast(`Weights set to ${u}`); }
      catch (e) { haptic.done(false); toast(e.message, true); }
    });
    unitSeg.append(b);
  }
  rows.append(row('Weight unit', null, unitSeg));

  // 2. Default rest (compact inline stepper with hold-to-repeat and poll-safe auto-save)
  let restVal = data.profile.rest_seconds;
  const restCtrl = el('div', 'mini-stepper');
  const valNode = el('span', '', `${restVal}s`);
  const stepBtn = (label, delta, aria) => {
    const b = el('button', '', label);
    b.type = 'button';
    b.setAttribute('aria-label', aria);
    let hold, held = false, startY = 0;
    const stop = () => { clearTimeout(hold); clearInterval(hold); };
    const bump = () => {
      const next = Math.max(15, Math.min(600, restVal + delta));
      if (next === restVal) { stop(); return; }
      haptic.select();
      restVal = next;
      valNode.textContent = `${restVal}s`;
      minus.disabled = restVal <= 15;
      plus.disabled = restVal >= 600;
      clearTimeout(restSaveTimer);
      restSaveTimer = setTimeout(async () => {
        restSaveTimer = null;
        restSaving = true;
        try { await toolCall('profile_update', { changes: { rest_seconds: restVal } }); haptic.done(); toast(`Default rest ${restVal}s`); }
        catch (e) { haptic.done(false); toast(e.message, true); }
        finally { restSaving = false; }
      }, 450);
    };
    b.addEventListener('pointerdown', event => {
      if (event.button) return;
      held = false;
      startY = event.clientY;
      hold = setTimeout(() => { held = true; bump(); hold = setInterval(bump, 120); }, 420);
    });
    b.addEventListener('pointermove', event => { if (Math.abs(event.clientY - startY) > 10) stop(); });
    ['pointerup', 'pointerleave', 'pointercancel'].forEach(t => b.addEventListener(t, stop));
    b.addEventListener('click', () => { if (!held) bump(); held = false; });
    return b;
  };
  const minus = stepBtn('−', -15, 'Less rest');
  const plus = stepBtn('+', 15, 'More rest');
  minus.disabled = restVal <= 15;
  plus.disabled = restVal >= 600;
  restCtrl.append(minus, valNode, plus);
  rows.append(row('Default rest', 'Between working sets', restCtrl));

  // 3. Reminders & weekly plan (On | Paused segmented control)
  const remSeg = el('div', 'segmented');
  for (const [label, want] of [['On', true], ['Paused', false]]) {
    const b = el('button', '', label);
    b.type = 'button';
    b.setAttribute('aria-pressed', String(on === want));
    b.addEventListener('click', async () => {
      if (on === want) return;
      haptic.select();
      try {
        if (want) await toolCall('runtime_resume', { owner_asked: true });
        else await toolCall('runtime_pause', {});
        haptic.done();
        toast(want ? 'Reminders on' : 'Reminders paused');
      } catch (e) { haptic.done(false); toast(e.message, true); }
    });
    remSeg.append(b);
  }
  rows.append(row('Reminders & plan', `${data.profile.weekly_target_sessions}/wk · ${data.profile.earliest_workout_start}–${data.profile.latest_workout_finish}`, remSeg));

  // 4. Timezone (show current IANA zone; 1-tap device sync when different, or edit in sheet)
  const browserTz = (() => { try { return Intl.DateTimeFormat().resolvedOptions().timeZone || ''; } catch { return ''; } })();
  const tzCtrl = document.createDocumentFragment();
  if (browserTz && browserTz !== data.profile.timezone) {
    const sync = el('button', 'chip sm', 'Use device');
    sync.type = 'button';
    sync.addEventListener('click', async () => {
      haptic.select();
      try { await toolCall('profile_update', { changes: { timezone: browserTz } }); haptic.done(); toast(`Timezone set to ${browserTz}`); }
      catch (e) { haptic.done(false); toast(e.message, true); }
    });
    tzCtrl.append(sync);
  }
  const tzEdit = el('button', 'chip sm', 'Change');
  tzEdit.type = 'button';
  tzEdit.addEventListener('click', () => openTimezoneEditor(data.profile.timezone, browserTz));
  tzCtrl.append(tzEdit);
  rows.append(row('Timezone', data.profile.timezone, tzCtrl));

  // 5. Calendar ICS subscription URL
  if (data.ics_path) {
    const copy = el('button', 'chip sm', 'Copy');
    copy.type = 'button';
    copy.addEventListener('click', async () => {
      const url = new URL(data.ics_path, location.href).href;
      try {
        await navigator.clipboard.writeText(url);
        haptic.done();
        copy.textContent = 'Copied ✓';
        setTimeout(() => { copy.textContent = 'Copy'; }, 1800);
        toast('Calendar link copied');
      } catch { window.prompt('Calendar subscription URL:', url); }
    });
    rows.append(row('Calendar feed', 'Read-only · Apple & Google', copy));
  }
  host.append(rows);
}

/* ---------- Shell ---------- */

function activeTab() {
  const tab = location.hash.slice(1);
  return ['today', 'progress', 'history', 'exercises', 'memory'].includes(tab) ? tab : 'today';
}

function showTab() {
  const tab = activeTab();
  document.querySelectorAll('.tabs button').forEach(button => button.toggleAttribute('aria-current', button.dataset.tab === tab));
  document.querySelectorAll('.tab').forEach(section => { section.hidden = section.id !== `tab-${tab}`; });
  hideTip();
  syncBack();
  syncMain();
  // Today decides it (renderToday); any other tab has its tab bar.
  if (tab !== 'today') {
    document.documentElement.classList.remove('live-mode');
    queueActiveId = null;
  }
  // The browser's title line names the tab; in Telegram it's hidden.
  document.querySelector('h1').textContent = { today: 'Today', progress: 'Progress', history: 'History', exercises: 'Exercises', memory: 'Settings' }[tab];
  if (!state) return;
  // Charts measure their container, so draw a tab only once it's visible.
  if (tab === 'today') renderToday(state);
  if (tab === 'progress' && state.insights) renderInsights(state);
  if (tab === 'history' && state.history) renderHistory(state.history);
  if (tab === 'exercises' && state.plans) renderExercises();
  if (tab === 'memory' && state.memory) { renderSettings(state); renderMemory(state.memory); renderExtensions(state.extensions); }
  pruneImgObserver();
}

let rendered = '';
function render(data, checkedAt = null) {
  mergePreviews(data.previews);
  const zone = { timeZone: data.timezone, hour: '2-digit', minute: '2-digit', hour12: false };
  const stamp = checkedAt ? new Date(checkedAt) : new Date(data.captured_at);
  document.getElementById('updated').textContent = `Updated ${stamp.toLocaleTimeString('en-GB', zone)}`;
  // Auto-refresh mostly returns the same data: then only the time changes, and open rows, the
  // tooltip and the scroll position stay as they are.
  const content = JSON.stringify({ ...data, captured_at: null });
  if (content === rendered) return;
  rendered = content;
  state = data;
  showTab();
}

let failures = 0;
let freshState = false;
async function refresh() {
  try {
    const response = await fetch('/api/state', { headers: apiHeaders() });
    if (!response.ok) throw new Error('State unavailable');
    const checkedAt = response.headers.get('Date') || Date.now();
    const data = await response.json();
    freshState = true;
    render(data, checkedAt);
    if (pendingSwapRef) {
      const swap = data.workout?.live?.swap;
      const matches = swap?.ref === pendingSwapRef;
      pendingSwapRef = null;
      history.replaceState(null, '', `${location.pathname}${location.hash}`);
      if (matches) openSwapPicker(swap);
      else {
        closeAfterSwap = false;
        toast(data.workout ? 'That exercise has moved on.' : 'That workout has ended.');
      }
    }
    failures = 0;
    document.getElementById('load-error').hidden = true;
    try { localStorage.setItem('state', JSON.stringify(data)); } catch { /* private mode: no instant paint next time */ }
    return true;
  } catch {
    // One missed poll is noise (a lift, a lift's dead zone); two in a row are worth saying.
    failures += 1;
    document.getElementById('load-error').hidden = failures < 2 && Boolean(state);
    return false;
  }
}
// Paint the last state at once, then refresh: opening the app in a basement shows something right away.
try {
  const cached = JSON.parse(localStorage.getItem('state') || 'null');
  if (cached) render(cached);
} catch { /* nothing cached */ }
if (!state) showTab();

/* Close the exercise sheet (×, backdrop, Esc, Telegram's back button), asking first if a plan edit
   isn't saved. */
function closeSheet() {
  const dialog = document.getElementById('ex-detail');
  if (!dirtyForm) return dialog.close();
  const ask = 'Discard the unsaved plan change?';
  if (tgHas('6.2')) tg.showConfirm(ask, ok => ok && dialog.close());
  else if (window.confirm(ask)) dialog.close();
}

/* Switch tabs. The hash names the tab (a reload stays put) but adds no history entry, so the phone's back
   gesture leaves the app instead of stepping through every tab visited. */
function goTab(tab) {
  if (activeTab() === tab) return;
  haptic.select();
  history.replaceState(null, '', `#${tab}`);
  showTab();
  // Like any app's tab bar: a tab opens at its top.
  window.scrollTo(0, 0);
}
document.querySelectorAll('.tabs button').forEach(button => button.addEventListener('click', () => goTab(button.dataset.tab)));
// The exercise sheet closes with ×, the backdrop, Esc or Telegram's back button; an unsaved edit goes with it.
document.getElementById('ex-detail').addEventListener('close', () => { clearDirty(); syncBack(); });
// In Telegram the client downloads the CSV itself; a download link in its web view does nothing on iOS.
document.getElementById('export').addEventListener('click', event => {
  const url = new URL('/export.csv', location.href);
  if (tg?.initData) url.searchParams.set('init_data', tg.initData);
  if (!tgHas('8.0')) { if (tg?.initData) event.currentTarget.href = url.href; return; }
  event.preventDefault();
  tg.downloadFile({ url: url.href, file_name: 'gymclaw-sets.csv' });
});
document.getElementById('exercise-select').addEventListener('change', event => { selectedExercise = event.target.value; renderProgress(state.insights); });
window.addEventListener('hashchange', showTab);
document.querySelectorAll('#ex-views button').forEach(button => button.addEventListener('click', () => { haptic.select(); exView.view = button.dataset.ex; renderExercises(); }));
document.getElementById('ex-search').addEventListener('input', event => { exView.query = event.target.value; renderExercises(); });
// A tap on the dimmed backdrop (the dialog itself, outside its sheet) closes it.
document.getElementById('ex-detail').addEventListener('click', event => { if (event.target === event.currentTarget) closeSheet(); });
document.getElementById('ex-detail').addEventListener('cancel', event => { event.preventDefault(); closeSheet(); });
let resizeTimer;
window.addEventListener('resize', () => { clearTimeout(resizeTimer); resizeTimer = setTimeout(showTab, 150); });
window.addEventListener('load', () => {
  const activeId = state?.workout?.active?.id ?? null;
  if (activeTab() === 'today' && activeId && queueActiveId !== activeId) {
    scrollQueueToCurrent(document.querySelector('#focus .queue'), activeId);
  }
});
document.querySelectorAll('.crowd-nav .segmented button').forEach(button => button.addEventListener('click', () => {
  haptic.select();
  crowdView.view = button.dataset.view; crowdView.back = 0; renderAttendance(state.crowd);
}));
document.getElementById('crowd-prev').addEventListener('click', () => { crowdView.back += 1; renderAttendance(state.crowd); });
document.getElementById('crowd-next').addEventListener('click', () => { crowdView.back = Math.max(-CROWD_AHEAD[crowdView.view], crowdView.back - 1); renderAttendance(state.crowd); });
document.getElementById('retry').addEventListener('click', () => refresh());
// Signal ready only after the initial tab is revealed and listeners are wired.
if (tg) tgTry(() => tg.ready());
if (!state && activeTab() === 'today') {
  // Cold start on Today: fire /api/today (~3 KB) and full /api/state in parallel so Today paints
  // immediately on slow Wi-Fi without delaying full-state hydration by an extra round trip.
  refresh();
  fetch('/api/today', { headers: apiHeaders() })
    .then(r => (r.ok ? r.json().then(today => ({ today, checkedAt: r.headers.get('Date') || Date.now() })) : null))
    .then(res => { if (res && !freshState) render(res.today, res.checkedAt); })
    .catch(() => {});
} else {
  refresh();
}
// Live state refreshes itself every 10 s while the page is visible (a refresh is ~70 ms server-side),
// and at once when you come back to the tab. Telegram keeps a minimized Mini App's page running and
// visible, so there it polls only while the app is active.
let active = true;
if (tgHas('8.0')) {
  tg.onEvent('activated', () => { active = true; refresh(); });
  tg.onEvent('deactivated', () => { active = false; });
}
async function refreshLive() {
  if (sending) return true;
  // If we only have a partial /api/today snapshot (e.g. initial /api/state failed), self-heal to full state.
  if (!state?.plans) return refresh();
  try {
    const response = await fetch('/api/live', { headers: apiHeaders() });
    if (!response.ok) throw new Error('Live unavailable');
    const checkedAt = response.headers.get('Date') || Date.now();
    const live = await response.json();
    if (sending || (state?.captured_at && live.captured_at < state.captured_at)) return true;
    // If a workout ended or started, pull full state so History/KPIs catch up too.
    if (Boolean(live.workout) !== Boolean(state?.workout)) return refresh();
    render({ ...state, ...live }, checkedAt);
    failures = 0;
    document.getElementById('load-error').hidden = true;
    return true;
  } catch {
    failures += 1;
    document.getElementById('load-error').hidden = failures < 2 && Boolean(state);
    return false;
  }
}

// Mid-workout every 3 s via lightweight /api/live; otherwise full /api/state every 10 s.
let lastPoll = Date.now();
setInterval(() => {
  const live = Boolean(state?.workout && activeTab() === 'today');
  if (!active || document.hidden || Date.now() - lastPoll < (live ? 3000 : 10000)) return;
  lastPoll = Date.now();
  if (live) refreshLive();
  else refresh();
}, 1000);
document.addEventListener('visibilitychange', () => !document.hidden && refresh());
