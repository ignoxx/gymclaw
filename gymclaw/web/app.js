/* Read-only view of GymClaw state. Presentation only: no timers or writes on the server. */
const SVG = 'http://www.w3.org/2000/svg';
const WEEKDAYS = ['Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat', 'Sun'];
let restTimer = null;
let state = null;
let selectedExercise = null;
// Snapshot clock minus browser clock: the demo is frozen in time, live is ~0.
let clockOffset = 0;
const now = () => Date.now() + clockOffset;

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

const minutesOf = time => { const [h, m] = time.split(':').map(Number); return h * 60 + m; };
const clock = minutes => `${String(Math.floor(minutes / 60) % 24).padStart(2, '0')}:${String(Math.round(minutes % 60)).padStart(2, '0')}`;
const tonnes = kg => kg >= 1000 ? `${(kg / 1000).toFixed(1)} t` : `${Math.round(kg)} kg`;
const kgs = value => `${Number(value)} kg`;

function crowdLabel(session) {
  if (session.crowd) return session.crowd;
  return session.count !== null && session.count !== undefined ? `~${session.count} ppl` : null;
}

function thumb(url) {
  const image = el('img');
  image.alt = '';
  if (url) image.src = url;
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
  tooltip.style.top = `${event.clientY - box.height - 12 + window.scrollY}px`;
}
const hideTip = () => { tooltip.hidden = true; };

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
  hit.addEventListener('pointermove', event => {
    const rect = chart.svg.getBoundingClientRect();
    const px = (event.clientX - rect.left) * chart.W / rect.width;
    let index = 0;
    xs.forEach((x, i) => { if (Math.abs(x - px) < Math.abs(xs[index] - px)) index = i; });
    const info = describe(index);
    line.setAttribute('x1', xs[index]); line.setAttribute('x2', xs[index]); line.setAttribute('visibility', 'visible');
    if (info.y !== undefined) { dot.setAttribute('cx', xs[index]); dot.setAttribute('cy', info.y); dot.setAttribute('visibility', 'visible'); }
    showTip(event, info.title, info.rows);
  });
  hit.addEventListener('pointerleave', () => { line.setAttribute('visibility', 'hidden'); dot.setAttribute('visibility', 'hidden'); hideTip(); });
}

/* ---------- Today ---------- */

function renderWeek(days, range) {
  const week = document.getElementById('week');
  week.replaceChildren();
  for (const day of days) {
    const column = el('div', `day${day.today ? ' today' : ''}`);
    const heading = el('div', 'day-heading');
    heading.append(el('span', '', day.today ? 'Today' : day.name), el('strong', '', day.date));
    const session = day.session;
    const slot = el('div', 'slot');
    if (session) {
      slot.classList.add('training', session.status.toLowerCase());
      slot.append(el('b', '', session.start), el('span', '', session.template));
      const crowd = crowdLabel(session);
      if (crowd) slot.append(el('em', `crowd ${crowdClass(session.crowd)}`, crowd));
    } else {
      slot.classList.add('rest');
      slot.setAttribute('aria-label', 'Rest day');
    }
    column.append(heading, slot);
    week.append(column);
  }
  const count = days.filter(day => day.session).length;
  document.getElementById('range').textContent = range;
  document.getElementById('session-count').textContent = `${count} session${count === 1 ? '' : 's'}`;
}

/* ---------- Gym attendance ----------
   Times are local wall-clock minutes since 1970 (server-side, profile timezone), so a
   Date read with getUTC* gives local calendar fields without any timezone math. */
const DAY = 1440;
const crowdView = { view: 'today', back: 0 };
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
  return back === 0 ? 'Today' : back === 1 ? 'Yesterday' : dayLabel(start);
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
  document.querySelectorAll('.segmented button').forEach(button => button.setAttribute('aria-pressed', String(button.dataset.view === view)));
  document.getElementById('crowd-title').textContent = crowdTitle(view, back, window_);
  document.getElementById('crowd-prev').disabled = !series.length || series[0][0] >= start;
  document.getElementById('crowd-next').disabled = back === 0;
  const raw = series.filter(([m]) => m >= start && m < end);
  const points = view === 'month' ? hourlyMeans(raw) : raw;
  const typical = view === 'today' ? typicalDay(series, start).filter(([m]) => m < end) : [];
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
  if (!points.length && typical.length < 2) {
    box.replaceChildren(el('p', 'empty', 'No readings in this period.'));
    return;
  }
  const all = [...points, ...typical];
  const chart = frame(box, niceTop(Math.max(...all.map(p => p[1]))), { label: `Reported check-ins, ${crowdTitle(view, back, window_)}` });
  const axisEnd = Math.max(end, start + 60);
  const x = m => chart.L + (m - start) / (axisEnd - start) * (chart.W - chart.L - chart.R);
  for (const [m, label] of axisTicks(view, start, axisEnd, chart.W)) chart.svg.append(svgText(x(m), chart.H - 6, label, { 'text-anchor': 'middle' }));
  if (view === 'week') for (let m = start + DAY; m < end; m += DAY) chart.svg.append(svgEl('line', { x1: x(m), x2: x(m), y1: chart.T, y2: chart.H - chart.B, class: 'grid' }));
  // Gaps longer than an hour (or two buckets) break the line instead of bridging it.
  const gap = view === 'month' ? 120 : 60;
  const runs = list => list.reduce((acc, point, i) => { if (!i || point[0] - list[i - 1][0] > gap) acc.push([]); acc.at(-1).push(point); return acc; }, []);
  const path = run => run.map(([m, v], i) => `${i ? 'L' : 'M'}${x(m).toFixed(1)},${chart.y(v).toFixed(1)}`).join(' ');
  for (const run of runs(typical)) if (run.length > 1) chart.svg.append(svgEl('path', { d: path(run), class: 'typical' }));
  for (const run of runs(points)) {
    chart.svg.append(svgEl('path', { d: `${path(run)} L${x(run.at(-1)[0])},${chart.y(0)} L${x(run[0][0])},${chart.y(0)} Z`, class: 'area' }));
    chart.svg.append(svgEl('path', { d: path(run), class: 'line' }));
  }
  if (points.length && back === 0 && view !== 'month') {
    const [m, v] = points.at(-1);
    chart.svg.append(svgEl('circle', { cx: x(m), cy: chart.y(v), r: 4, class: 'dot' }));
  }
  const typicalAt = m => typical.find(([t]) => Math.floor(t / 60) === Math.floor(m / 60))?.[1];
  const hover = points.length ? points : typical;
  crosshair(chart, hover.map(([m]) => x(m)), index => {
    const [m, v] = hover[index];
    const time = view === 'month' ? `${clock(m % DAY - 30)}–${clock(m % DAY + 30)}` : clock(m % DAY);
    const title = view === 'today' ? (points.length ? time : `${clock(m % DAY - 30)}–${clock(m % DAY + 30)}`) : `${dayLabel(m)} ${time}`;
    const rows = points.length ? [[v, view === 'month' ? 'avg checked in' : 'checked in', 'today']] : [];
    const usual = typicalAt(m);
    if (usual !== undefined) rows.push([usual, `typical ${dateLabel(start, { weekday: 'long' })}`, 'typical']);
    return { title, rows, y: chart.y(v) };
  });
}

function renderGym(crowd) {
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
  // Repeats in a row collapse into one line ("Calendar updated ×3").
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
  const tick = () => {
    const left = Math.max(0, Math.ceil((Date.parse(until) - now()) / 1000));
    node.textContent = left ? `Rest ${Math.floor(left / 60)}:${String(left % 60).padStart(2, '0')}` : 'Rest over';
    if (!left) clearInterval(restTimer);
  };
  tick();
  restTimer = setInterval(tick, 1000);
}

function renderWorkout(focus, workout) {
  const active = workout.active;
  const heading = el('div', 'section-heading');
  heading.append(el('span', '', `${workout.template} · in progress`), el('span', '', `Finish ~${workout.eta}`));
  focus.append(heading);
  if (!active) {
    focus.append(el('h2', 'title', 'Wrapping up'));
  } else {
    focus.append(el('h2', 'title', active.name));
    const pose = el('div', 'pose');
    pose.append(thumb(workout.exercises.find(e => e.id === active.id)?.svg_url));
    focus.append(pose);
    const target = el('div', 'target');
    target.append(el('strong', '', active.rep_min === active.rep_max ? active.rep_min : `${active.rep_min}–${active.rep_max}`), el('span', '', 'reps'));
    if (active.weight) target.append(el('span', 'times', '×'), el('strong', '', active.weight), el('span', '', 'kg'));
    focus.append(target);
    const status = el('div', 'status');
    const label = el('strong', '', active.set_type === 'WARMUP' ? 'Warm-up' : `Set ${active.set_number} of ${active.sets}`);
    status.append(label, el('span', '', workout.last_set ? `Last ${workout.last_set}` : ''));
    if (workout.rest_until) {
      status.classList.add('resting');
      startRest(label, workout.rest_until);
    }
    focus.append(status);
  }
  const queue = el('div', 'queue');
  for (const exercise of workout.exercises) {
    const item = el('div', `exercise ${exercise.status.toLowerCase()}${exercise.id === active?.id ? ' current' : ''}`);
    item.append(thumb(exercise.svg_url), el('span', '', exercise.name), el('small', '', `${exercise.done}/${exercise.sets}`));
    queue.append(item);
  }
  focus.append(queue);
}

function renderNext(focus, next) {
  const heading = el('div', 'section-heading');
  heading.append(el('span', '', `Next · ${next.when}`), el('span', '', `Leave ${next.leave}`));
  focus.append(heading, el('h2', 'title', next.template));
  const crowd = crowdLabel(next);
  if (crowd) focus.append(el('p', 'subtitle', next.crowd && next.count ? `${next.crowd} · ~${next.count} people expected` : `${crowd} expected`));
  const list = el('ol', 'plan');
  for (const exercise of next.exercises) {
    const row = el('li');
    const text = el('div');
    text.append(el('strong', '', exercise.name), el('span', '', `${exercise.sets} sets of ${exercise.reps}`));
    row.append(thumb(exercise.svg_url), text, el('em', '', exercise.last ? `Last ${exercise.last}` : ''));
    list.append(row);
  }
  focus.append(list);
}

function renderToday(data) {
  renderWeek(data.days, data.range);
  renderGym(data.crowd);
  renderActivity(data.activity, data.days[0]?.name);
  const focus = document.getElementById('focus');
  focus.replaceChildren();
  clearInterval(restTimer);
  if (data.workout) renderWorkout(focus, data.workout);
  else if (data.next) renderNext(focus, data.next);
  else focus.append(el('h2', 'title', 'No sessions planned'));
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
    hit.addEventListener('pointermove', event => showTip(event, `Week of ${week.label}`, [[`${week.count} of ${insights.target}`, 'sessions']]));
    hit.addEventListener('pointerleave', hideTip);
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
  const points = series.find(s => s.id === selectedExercise).points;
  // Bodyweight exercises have no weight to estimate from: plot reps instead.
  const bodyweight = points.every(p => !p.weight);
  const value = p => bodyweight ? p.reps : p.e1rm;
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
  const max = Math.max(...insights.muscles.map(m => m.volume));
  for (const muscle of insights.muscles) {
    const row = el('div', 'bar-row');
    const track = el('div', 'bar-track');
    const fill = el('div', 'bar-fill');
    fill.style.width = `${Math.max(2, muscle.volume / max * 100)}%`;
    track.append(fill);
    row.append(el('span', 'bar-label', muscle.muscle), track, el('span', 'bar-value', tonnes(muscle.volume)));
    row.addEventListener('pointermove', event => showTip(event, muscle.muscle, [[tonnes(muscle.volume), 'volume'], [muscle.sets, 'working sets']]));
    row.addEventListener('pointerleave', hideTip);
    box.append(row);
  }
}

function renderHeatmap(insights) {
  const box = document.getElementById('heatmap');
  box.replaceChildren();
  const cells = insights.heatmap;
  const label = document.getElementById('quiet-label');
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
        node.addEventListener('pointermove', event => showTip(event, `${name} ${clock(h * 60)}–${clock(h * 60 + 60)}`, [[`~${cell.count}`, 'checked in'], [cell.days, cell.days === 1 ? 'day of data' : 'days of data']]));
        node.addEventListener('pointerleave', hideTip);
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
    box.replaceChildren(el('p', 'empty', 'Send GymClaw a photo of your scale to start tracking.'));
    return;
  }
  const values = points.flatMap(p => [p.kg, p.avg]);
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
  points.forEach(p => chart.svg.append(svgEl('circle', { cx: x(p.date), cy: y(p.kg), r: 3, class: 'weigh-in' })));
  chart.svg.append(svgEl('path', { class: 'line', d: points.map((p, i) => `${i ? 'L' : 'M'}${x(p.date).toFixed(1)},${y(p.avg).toFixed(1)}`).join(' ') }));
  crosshair(chart, points.map(p => x(p.date)), i => ({ title: points[i].label,
    rows: [[kgs(points[i].kg), 'weigh-in', 'weigh-in'], [kgs(points[i].avg), '7-day average', 'today']], y: y(points[i].avg) }));
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

// Crowd label as a CSS class: "A few" → "a-few".
const crowdClass = label => String(label || '').toLowerCase().replace(/\s+/g, '-');

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
    what.append(el('strong', '', workout.template), el('span', '', [`${workout.minutes} min`, `${workout.sets} sets`, tonnes(workout.volume)].join(' · ')));
    const tags = el('div', 'tags');
    if (workout.prs.length) tags.append(el('em', 'tag pr', `${workout.prs.length} PR${workout.prs.length > 1 ? 's' : ''}`));
    if (workout.crowd) tags.append(el('em', `tag crowd ${crowdClass(workout.crowd)}`, workout.crowd));
    summary.append(when, what, tags);
    const table = el('div', 'sets');
    for (const exercise of workout.exercises) {
      const row = el('div', 'set-row');
      const name = el('span', 'set-name', exercise.name);
      if (workout.prs.includes(exercise.name)) name.append(el('em', 'tag pr', 'PR'));
      row.append(name, el('span', 'set-list', exercise.sets.map(s => `${s.reps}×${s.weight}`).join('   ')));
      table.append(row);
    }
    item.append(summary, table);
    box.append(item);
  }
}

/* ---------- Shell ---------- */

function activeTab() {
  const tab = location.hash.slice(1);
  return ['today', 'progress', 'history'].includes(tab) ? tab : 'today';
}

function showTab() {
  const tab = activeTab();
  document.querySelectorAll('.tabs button').forEach(button => button.toggleAttribute('aria-current', button.dataset.tab === tab));
  document.querySelectorAll('.tab').forEach(section => { section.hidden = section.id !== `tab-${tab}`; });
  hideTip();
  if (!state) return;
  // Charts measure their container, so draw a tab only once it's visible.
  if (tab === 'today') renderToday(state);
  if (tab === 'progress') renderInsights(state);
  if (tab === 'history') renderHistory(state.history);
}

let rendered = '';
function render(data) {
  if (typeof data.demo !== 'boolean' || data.live_database_accessed !== !data.demo) throw new Error('Invalid state');
  const zone = { timeZone: data.timezone, hour: '2-digit', minute: '2-digit', hour12: false };
  document.getElementById('updated').textContent = `${data.demo ? 'Demo · ' : ''}Updated ${new Date(data.captured_at).toLocaleTimeString('en-GB', zone)}`;
  // Auto-refresh mostly returns the same data: then only the time changes, and open rows, the
  // tooltip and the scroll position stay as they are.
  const content = JSON.stringify({ ...data, captured_at: null });
  if (content === rendered) return;
  rendered = content;
  state = data;
  renderGym(data.crowd);
  showTab();
}

let live = false;
async function refresh() {
  try {
    const response = await fetch('/api/state');
    if (!response.ok) throw new Error('State unavailable');
    const data = await response.json();
    live = !data.demo;
    clockOffset = live ? 0 : Date.parse(data.captured_at) - Date.now();
    render(data);
    document.getElementById('load-error').hidden = true;
  } catch {
    document.getElementById('load-error').hidden = false;
  }
}

document.querySelectorAll('.tabs button').forEach(button => button.addEventListener('click', () => { location.hash = button.dataset.tab; }));
document.getElementById('exercise-select').addEventListener('change', event => { selectedExercise = event.target.value; renderProgress(state.insights); });
window.addEventListener('hashchange', showTab);
let resizeTimer;
window.addEventListener('resize', () => { clearTimeout(resizeTimer); resizeTimer = setTimeout(showTab, 150); });
document.querySelectorAll('.segmented button').forEach(button => button.addEventListener('click', () => {
  crowdView.view = button.dataset.view; crowdView.back = 0; renderAttendance(state.crowd);
}));
document.getElementById('crowd-prev').addEventListener('click', () => { crowdView.back += 1; renderAttendance(state.crowd); });
document.getElementById('crowd-next').addEventListener('click', () => { crowdView.back = Math.max(0, crowdView.back - 1); renderAttendance(state.crowd); });
refresh();
// Live state refreshes itself every 10 s while the page is visible (a refresh is ~70 ms server-side),
// and at once when you come back to the tab. The demo is static.
setInterval(() => live && !document.hidden && refresh(), 10000);
document.addEventListener('visibilitychange', () => live && !document.hidden && refresh());
