/* This view changes presentation only. No timers or fitness mutations. */
function element(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function svgElement(tag, attributes) {
  const node = document.createElementNS('http://www.w3.org/2000/svg', tag);
  Object.entries(attributes).forEach(([key, value]) => node.setAttribute(key, String(value)));
  return node;
}

function attendanceChart(readings) {
  const svg = svgElement('svg', { viewBox: '0 0 540 150', role: 'img', 'aria-label': 'Synthetic reported check-in counts by retrieval time' });
  const top = Math.ceil(Math.max(...readings.map(item => item.count), 1) / 10) * 10;
  const points = readings.map((item, index) => [30 + index * (495 / Math.max(1, readings.length - 1)), 112 - item.count / top * 96]);
  [0, top / 2, top].forEach(value => {
    const y = 112 - value / top * 96;
    svg.append(svgElement('line', { x1: 25, x2: 529, y1: y, y2: y, class: 'chart-grid' }));
    const label = svgElement('text', { x: 0, y: y + 3, class: 'chart-label' });
    label.textContent = value;
    svg.append(label);
  });
  const line = points.map(([x, y], index) => `${index ? 'L' : 'M'}${x},${y}`).join(' ');
  svg.append(svgElement('path', { d: `${line} L525,112 L30,112 Z`, class: 'chart-area' }));
  svg.append(svgElement('path', { d: line, class: 'chart-line' }));
  points.forEach(([x, y], index) => {
    svg.append(svgElement('circle', { cx: x, cy: y, r: index === points.length - 1 ? 4 : 3, class: `chart-dot${index === points.length - 1 ? ' last' : ''}` }));
    const label = svgElement('text', { x, y: 139, 'text-anchor': 'middle', class: 'chart-label' });
    label.textContent = readings[index].time;
    svg.append(label);
  });
  document.getElementById('attendance-chart').append(svg);
}

function showExercise(exercise, workout) {
  const current = exercise.id === workout.active?.exercise_id;
  document.getElementById('exercise-state').textContent = current ? (workout.active.set_type === 'WARMUP' ? 'Warm-up' : 'Next set') : 'Exercise preview';
  document.getElementById('exercise-name').textContent = exercise.name;
  document.getElementById('set-position').textContent = current ? `Set ${workout.active.set_number} / ${workout.active.working_sets}` : `${exercise.working_sets} sets`;
  const image = document.getElementById('exercise-image');
  image.hidden = !exercise.svg_url;
  if (exercise.svg_url) image.src = exercise.svg_url;
  image.alt = `${exercise.name} illustration`;
  document.getElementById('weight').textContent = current ? workout.active.target_weight : exercise.target_weight;
  document.getElementById('reps').textContent = current ? `${workout.active.rep_min}–${workout.active.rep_max}` : `${exercise.rep_min}–${exercise.rep_max}`;
  document.getElementById('rest').hidden = !current || !workout.rest_intent_prepared;
  document.querySelector('.workout-footer').hidden = !current;
  document.querySelectorAll('.exercise').forEach(button => {
    const selected = button.dataset.exercise === exercise.id;
    button.classList.toggle('selected', selected);
    button.setAttribute('aria-pressed', String(selected));
  });
}

function render(data) {
  if (typeof data.demo !== 'boolean' || data.live_database_accessed !== !data.demo) throw new Error('Invalid state');
  document.querySelector('.demo-tag').textContent = data.demo ? 'Demo · synthetic data' : 'Live · read-only';
  document.getElementById('activity-mode').textContent = data.demo ? 'Demo' : 'Saved state';
  document.getElementById('data-notice').textContent = data.demo ? 'Read-only demo. Calendar writes off.' : `Read-only sandbox state. Calendar writes ${data.calendar_writes_enabled ? 'enabled' : 'off'}.`;
  document.getElementById('week-label').textContent = data.week;
  if (data.connections) {
    const connection = document.getElementById('connections');
    connection.hidden = false;
    connection.textContent = `Calendar ${data.connections.calendar.connected ? 'connected' : 'not configured'}. Telegram ${data.connections.telegram.enabled ? 'runtime enabled' : 'paused'}.`;
  }
  for (const id of ['week', 'attendance-chart', 'queue']) document.getElementById(id).replaceChildren();
  document.getElementById('session-count').textContent = `${data.sessions.length} sessions`;
  const week = document.getElementById('week');
  (data.week_days || ['Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat', 'Sun'].map((name, index) => ({ name, date: 12 + index }))).forEach(({ name: day, date }) => {
    const column = element('div', 'day');
    const heading = element('div', 'day-heading');
    heading.append(element('span', '', day), element('strong', '', date));
    const session = data.sessions.find(item => item.day === day);
    const slot = element('div', `slot ${session ? 'training' : 'recovery'}${session?.status === 'STARTED' ? ' current' : ''}`);
    if (session) {
      slot.append(element('b', '', session.start), element('span', '', session.template));
      slot.title = `Prep ${session.prep}, leave ${session.leave}, finish ${session.end}`;
    } else {
      slot.textContent = '·';
      slot.setAttribute('aria-label', `${day}, recovery day`);
    }
    column.append(heading, slot);
    week.append(column);
  });
  const latest = data.crowd.readings.at(-1);
  document.getElementById('attendance-count').textContent = latest ? latest.count : '—';
  const age = latest?.retrieved_at ? Math.max(0, Math.floor((Date.now() - Date.parse(latest.retrieved_at)) / 60000)) : null;
  document.getElementById('fetched-at').textContent = latest ? `Fetched ${latest.time}${age !== null ? `, ${age < 60 ? `${age} min` : `${Math.floor(age / 60)} h`} ago` : ''}` : 'No saved reading';
  document.querySelector('.chart-footer > span:last-child').textContent = data.crowd.personal_score !== null ? `Comfort ${(data.crowd.personal_score * 5).toFixed(1)} / 5, heuristic estimate` : 'Comfort score needs arrival feedback';
  document.querySelector('.sample-tag').textContent = data.demo ? 'Sample history' : 'Saved readings';
  document.getElementById('cadence').textContent = `${data.crowd.poll_every_minutes} min`;
  attendanceChart(data.crowd.readings);
  if (!data.demo) {
    document.querySelector('.cadence').title = data.crowd.polling_live ? 'Existing runtime polling enabled, guarded by training window' : 'Polling paused';
    document.getElementById('cadence').textContent = data.crowd.polling_live ? '15 min' : '15 min, paused';
    document.querySelector('.activity').replaceChildren(element('div', 'section-heading', 'Agent activity'));
    for (const event of data.activity) {
      const row = element('div', 'event');
      row.append(element('time', '', event.time), element('span', 'event-dot'), element('span', '', event.text));
      document.querySelector('.activity').append(row);
    }
  }
  const workout = data.workout;
  document.querySelector('.pose').hidden = !workout;
  document.querySelector('.prescription').hidden = !workout;
  document.querySelector('.telegram').hidden = !workout;
  document.getElementById('rest').hidden = !workout;
  document.querySelector('.workout-footer').hidden = !workout;
  if (!workout) {
    document.getElementById('exercise-name').textContent = 'Start a workout in Telegram';
    document.getElementById('exercise-state').textContent = 'No active workout';
    document.getElementById('set-position').textContent = '';
    document.querySelector('.layout').hidden = false;
    return;
  }
  document.getElementById('eta').textContent = workout.eta;
  document.querySelector('.workout-footer b').textContent = workout.last_set ? `${workout.last_set.weight} × ${workout.last_set.reps}` : '—';
  document.getElementById('rest-duration').textContent = `${workout.rest_seconds}s`;
  document.querySelector('.rest > div > span').textContent = data.demo ? 'Demo clock paused' : 'Snapshot';
  document.querySelector('.saved').textContent = data.demo ? 'Saved' : workout.timer_activated ? 'Scheduled' : 'Local intent';
  const queue = document.getElementById('queue');
  workout.exercises.forEach(exercise => {
    const button = element('button', 'exercise');
    button.dataset.exercise = exercise.id;
    button.setAttribute('aria-label', `Preview ${exercise.name}`);
    const image = element('img');
    if (exercise.svg_url) image.src = exercise.svg_url;
    image.alt = '';
    button.append(image, element('span', '', exercise.name));
    button.addEventListener('click', () => showExercise(exercise, workout));
    queue.append(button);
  });
  const exercise = workout.exercises.find(exercise => exercise.id === workout.active?.exercise_id) || workout.exercises[0];
  if (exercise) showExercise(exercise, workout);
  document.querySelector('.layout').hidden = false;
}

async function refresh() {
  const button = document.getElementById('refresh');
  button.disabled = true;
  try {
    const response = await fetch('/api/state');
    if (!response.ok) throw new Error('State unavailable');
    render(await response.json());
    document.getElementById('load-error').hidden = true;
  } catch {
    document.getElementById('load-error').hidden = false;
  } finally {
    button.disabled = false;
  }
}
document.getElementById('refresh').addEventListener('click', refresh);
refresh();
