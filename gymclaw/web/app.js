/* Read-only view of GymClaw state. Presentation only: no timers or writes on the server. */
const SVG = 'http://www.w3.org/2000/svg';
let restTimer = null;
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

function ago(iso) {
  const minutes = Math.max(0, Math.round((now() - Date.parse(iso)) / 60000));
  if (minutes < 1) return 'just now';
  if (minutes < 60) return `${minutes} min ago`;
  return minutes < 1440 ? `${Math.floor(minutes / 60)} h ago` : `${Math.floor(minutes / 1440)} d ago`;
}

function minutesOf(time) {
  const [h, m] = time.split(':').map(Number);
  return h * 60 + m;
}

function crowdLabel(session) {
  if (session.crowd) return session.crowd;
  return session.count !== null && session.count !== undefined ? `~${session.count} ppl` : null;
}

function renderWeek(days) {
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
  document.getElementById('session-count').textContent = `${count} session${count === 1 ? '' : 's'}`;
}

function renderChart(crowd) {
  const box = document.getElementById('chart');
  box.replaceChildren();
  const readings = crowd.readings.map(r => [minutesOf(r.time), r.count]);
  // At most the last 16 hours, so a stray overnight reading doesn't stretch the axis.
  const today = readings.filter(([m]) => m >= (readings.at(-1)?.[0] ?? 0) - 16 * 60);
  const typical = crowd.typical.map(t => [t.hour * 60 + 30, t.count]);
  const all = [...today, ...typical];
  if (!all.length) {
    box.append(el('p', 'empty', 'No readings yet.'));
    return;
  }
  const start = Math.floor(Math.min(...all.map(p => p[0])) / 60) * 60;
  const end = Math.max(start + 120, Math.ceil(Math.max(...all.map(p => p[0])) / 60) * 60);
  const peak = Math.max(...all.map(p => p[1]));
  const top = peak > 10 ? Math.ceil(peak / 10) * 10 : 10;
  // Draw at the container's real width so labels keep their size on phones.
  const W = Math.max(300, box.clientWidth || 540), H = 160, L = 26, R = 8, T = 8, B = 26;
  const x = m => L + (m - start) / (end - start) * (W - L - R);
  const y = v => T + (1 - v / top) * (H - T - B);
  const svg = svgEl('svg', { viewBox: `0 0 ${W} ${H}`, role: 'img', 'aria-label': 'Reported check-ins over the day' });
  for (const value of [0, top / 2, top]) {
    svg.append(svgEl('line', { x1: L, x2: W - R, y1: y(value), y2: y(value), class: 'grid' }));
    const label = svgEl('text', { x: L - 8, y: y(value) + 3, 'text-anchor': 'end', class: 'label' });
    label.textContent = value;
    svg.append(label);
  }
  const step = (end - start) / 60 > (W < 420 ? 4 : 8) ? 120 : 60;
  for (let m = start; m <= end; m += step) {
    const label = svgEl('text', { x: x(m), y: H - 6, 'text-anchor': 'middle', class: 'label' });
    label.textContent = `${String(Math.floor(m / 60) % 24).padStart(2, '0')}:00`;
    svg.append(label);
  }
  // A gap of more than an hour between readings breaks the line instead of bridging it.
  const path = points => points.map(([m, v], i) => `${i && m - points[i - 1][0] <= 60 ? 'L' : 'M'}${x(m).toFixed(1)},${y(v).toFixed(1)}`).join(' ');
  if (typical.length > 1) svg.append(svgEl('path', { d: path(typical), class: 'typical' }));
  if (today.length) {
    const line = path(today);
    let run = [];
    for (const point of [...today, null]) {
      if (run.length && (!point || point[0] - run.at(-1)[0] > 60)) {
        svg.append(svgEl('path', { d: `${path(run)} L${x(run.at(-1)[0])},${y(0)} L${x(run[0][0])},${y(0)} Z`, class: 'area' }));
        run = [];
      }
      if (point) run.push(point);
    }
    svg.append(svgEl('path', { d: line, class: 'line' }));
    const [m, v] = today.at(-1);
    svg.append(svgEl('circle', { cx: x(m), cy: y(v), r: 4, class: 'dot' }));
  }
  box.append(svg);
}

function renderGym(crowd) {
  document.getElementById('count').textContent = crowd.now ? crowd.now.count : '—';
  document.getElementById('count-time').textContent = crowd.now ? `${crowd.now.time} · ${ago(crowd.now.at)}` : 'No readings yet';
  document.getElementById('polling').textContent = crowd.polling ? 'Every 15 min' : 'Polling paused';
  document.getElementById('legend-day').textContent = crowd.day || 'Today';
  const typical = document.getElementById('legend-typical');
  typical.hidden = crowd.typical.length < 2;
  typical.textContent = `Typical ${crowd.weekday || ''}`.trim();
  renderChart(crowd);
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
  for (const event of grouped) {
    const row = el('div', 'event');
    row.append(el('time', '', event.day === today ? event.time : `${event.day} ${event.time}`), el('span', 'event-dot'), el('span', '', event.count > 1 ? `${event.text} ×${event.count}` : event.text));
    list.append(row);
  }
}

function thumb(url) {
  const image = el('img');
  image.alt = '';
  if (url) image.src = url;
  return image;
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
    if (active.weight) target.append(el('strong', '', active.weight), el('span', '', 'kg'), el('span', 'times', '×'));
    target.append(el('strong', '', active.rep_min === active.rep_max ? active.rep_min : `${active.rep_min}–${active.rep_max}`), el('span', '', 'reps'));
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
    text.append(el('strong', '', exercise.name), el('span', '', `${exercise.sets} × ${exercise.reps}`));
    row.append(thumb(exercise.svg_url), text, el('em', '', exercise.last ? `Last ${exercise.last}` : ''));
    list.append(row);
  }
  focus.append(list);
}

function render(data) {
  if (typeof data.demo !== 'boolean' || data.live_database_accessed !== !data.demo) throw new Error('Invalid state');
  const zone = { timeZone: data.timezone, hour: '2-digit', minute: '2-digit', hour12: false };
  // Visible before drawing, so the chart can measure its real width.
  document.querySelector('.layout').hidden = false;
  const mode = document.getElementById('mode');
  mode.textContent = data.demo ? 'Demo' : 'Live';
  mode.classList.toggle('live', !data.demo);
  document.getElementById('updated').textContent = `Updated ${new Date(data.captured_at).toLocaleTimeString('en-GB', zone)}`;
  document.getElementById('data-notice').textContent = data.demo ? 'Synthetic demo data, generated by the real planner and workout code.' : 'Read-only view of your GymClaw sandbox.';
  document.getElementById('range').textContent = data.range;
  renderWeek(data.days);
  renderGym(data.crowd);
  renderActivity(data.activity, data.days[0]?.name);
  const focus = document.getElementById('focus');
  focus.replaceChildren();
  clearInterval(restTimer);
  if (data.workout) renderWorkout(focus, data.workout);
  else if (data.next) renderNext(focus, data.next);
  else focus.append(el('h2', 'title', 'No sessions planned'));
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
document.getElementById('refresh').addEventListener('click', refresh);
refresh();
// Live state refreshes itself; the demo is static.
setInterval(() => live && !document.hidden && refresh(), 60000);
