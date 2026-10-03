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
      if (crowd) slot.append(el('em', `crowd ${String(session.crowd || '').toLowerCase()}`, crowd));
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

function renderAttendance(crowd) {
  const box = document.getElementById('chart');
  const readings = crowd.readings.map(r => [minutesOf(r.time), r.count]);
  // At most the last 16 hours, so a stray overnight reading doesn't stretch the axis.
  const today = readings.filter(([m]) => m >= (readings.at(-1)?.[0] ?? 0) - 16 * 60);
  const typical = crowd.typical.map(t => [t.hour * 60 + 30, t.count]);
  const all = [...today, ...typical];
  if (!all.length) {
    box.replaceChildren(el('p', 'empty', 'No readings yet.'));
    return;
  }
  const start = Math.floor(Math.min(...all.map(p => p[0])) / 60) * 60;
  const end = Math.max(start + 120, Math.ceil(Math.max(...all.map(p => p[0])) / 60) * 60);
  const chart = frame(box, niceTop(Math.max(...all.map(p => p[1]))), { label: 'Reported check-ins over the day' });
  const x = m => chart.L + (m - start) / (end - start) * (chart.W - chart.L - chart.R);
  const step = (end - start) / 60 > (chart.W < 420 ? 4 : 8) ? 120 : 60;
  for (let m = start; m <= end; m += step) chart.svg.append(svgText(x(m), chart.H - 6, clock(m), { 'text-anchor': 'middle' }));
  // A gap of more than an hour between readings breaks the line instead of bridging it.
  const path = points => points.map(([m, v], i) => `${i && m - points[i - 1][0] <= 60 ? 'L' : 'M'}${x(m).toFixed(1)},${chart.y(v).toFixed(1)}`).join(' ');
  if (typical.length > 1) chart.svg.append(svgEl('path', { d: path(typical), class: 'typical' }));
  if (today.length) {
    let run = [];
    for (const point of [...today, null]) {
      if (run.length && (!point || point[0] - run.at(-1)[0] > 60)) {
        chart.svg.append(svgEl('path', { d: `${path(run)} L${x(run.at(-1)[0])},${chart.y(0)} L${x(run[0][0])},${chart.y(0)} Z`, class: 'area' }));
        run = [];
      }
      if (point) run.push(point);
    }
    chart.svg.append(svgEl('path', { d: path(today), class: 'line' }));
    const [m, v] = today.at(-1);
    chart.svg.append(svgEl('circle', { cx: x(m), cy: chart.y(v), r: 4, class: 'dot' }));
  }
  const typicalAt = minutes => typical.find(([m]) => Math.floor(m / 60) === Math.floor(minutes / 60))?.[1];
  const points = today.length ? today : typical;
  crosshair(chart, points.map(([m]) => x(m)), index => {
    const [m, v] = points[index];
    const usual = typicalAt(m);
    const rows = today.length ? [[v, 'checked in', 'today']] : [];
    if (usual !== undefined) rows.push([usual, `typical ${crowd.weekday || ''}`.trim(), 'typical']);
    return { title: today.length ? clock(m) : `${clock(m - 30)}–${clock(m + 30)}`, rows, y: chart.y(v) };
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
  document.getElementById('legend-day').textContent = crowd.day || 'Today';
  const typical = document.getElementById('legend-typical');
  typical.hidden = crowd.typical.length < 2;
  typical.textContent = `Typical ${crowd.weekday || ''}`.trim();
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
  const weights = points.map(p => p.weight);
  const low = Math.max(0, Math.floor((Math.min(...weights) * 0.9) / 5) * 5);
  const top = Math.ceil(Math.max(...weights) * 1.05 / 5) * 5 || 5;
  const chart = frame(box, top - low, { label: 'Top set weight per session' });
  // Shift labels: the axis starts at `low`, not 0, so small progress stays visible.
  chart.svg.querySelectorAll('text.label').forEach((label, i) => { label.textContent = Math.round(low + [0, (top - low) / 2, top - low][i]); });
  const y = weight => chart.y(weight - low);
  const xs = points.map((_, i) => points.length === 1 ? (chart.L + chart.W - chart.R) / 2 : chart.L + 12 + i * (chart.W - chart.L - chart.R - 24) / (points.length - 1));
  chart.svg.append(svgEl('path', { class: 'line', d: points.map((p, i) => `${i ? 'L' : 'M'}${xs[i]},${y(p.weight)}`).join(' ') }));
  points.forEach((p, i) => chart.svg.append(svgEl('circle', { cx: xs[i], cy: y(p.weight), r: 4, class: 'point' })));
  const every = Math.ceil(points.length / (chart.W < 420 ? 4 : 7));
  points.forEach((p, i) => { if (i % every === 0 || i === points.length - 1) chart.svg.append(svgText(xs[i], chart.H - 6, p.label.replace(/^\w+ /, ''), { 'text-anchor': 'middle' })); });
  crosshair(chart, xs, i => ({ title: points[i].label, rows: [[`${points[i].reps} × ${kgs(points[i].weight)}`, 'top set']], y: y(points[i].weight) }));
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

function renderInsights(data) {
  renderKpis(data.insights);
  renderWeeks(data.insights);
  renderProgress(data.insights);
  renderMuscles(data.insights);
  renderHeatmap(data.insights);
}

/* ---------- History ---------- */

function renderHistory(history) {
  const box = document.getElementById('history');
  box.replaceChildren();
  document.getElementById('history-count').textContent = `${history.length} workout${history.length === 1 ? '' : 's'}`;
  if (!history.length) {
    box.append(el('p', 'empty', 'No finished workouts yet.'));
    return;
  }
  for (const workout of history) {
    const item = el('details', 'workout');
    const summary = el('summary');
    const when = el('div', 'when');
    when.append(el('strong', '', workout.label), el('span', '', workout.time));
    const what = el('div', 'what');
    what.append(el('strong', '', workout.template), el('span', '', [`${workout.minutes} min`, `${workout.sets} sets`, tonnes(workout.volume)].join(' · ')));
    const tags = el('div', 'tags');
    if (workout.prs.length) tags.append(el('em', 'tag pr', `${workout.prs.length} PR${workout.prs.length > 1 ? 's' : ''}`));
    if (workout.crowd) tags.append(el('em', `tag crowd ${workout.crowd.toLowerCase()}`, workout.crowd));
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

function render(data) {
  if (typeof data.demo !== 'boolean' || data.live_database_accessed !== !data.demo) throw new Error('Invalid state');
  state = data;
  const zone = { timeZone: data.timezone, hour: '2-digit', minute: '2-digit', hour12: false };
  const mode = document.getElementById('mode');
  mode.textContent = data.demo ? 'Demo' : 'Live';
  mode.classList.toggle('live', !data.demo);
  document.getElementById('updated').textContent = `Updated ${new Date(data.captured_at).toLocaleTimeString('en-GB', zone)}`;
  document.getElementById('data-notice').textContent = data.demo ? 'Synthetic demo data, generated by the real planner and workout code.' : 'Read-only view of your GymClaw sandbox.';
  renderGym(data.crowd);
  showTab();
}

let live = false;
async function refresh() {
  const button = document.getElementById('refresh');
  button.disabled = true;
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
  } finally {
    button.disabled = false;
  }
}

document.querySelectorAll('.tabs button').forEach(button => button.addEventListener('click', () => { location.hash = button.dataset.tab; }));
document.getElementById('exercise-select').addEventListener('change', event => { selectedExercise = event.target.value; renderProgress(state.insights); });
window.addEventListener('hashchange', showTab);
let resizeTimer;
window.addEventListener('resize', () => { clearTimeout(resizeTimer); resizeTimer = setTimeout(showTab, 150); });
document.getElementById('refresh').addEventListener('click', refresh);
refresh();
// Live state refreshes itself; the demo is static.
setInterval(() => live && !document.hidden && refresh(), 60000);
