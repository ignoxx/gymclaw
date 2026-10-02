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
  const top = Math.ceil(Math.max(...readings.map(item => item.count)) / 10) * 10;
  const points = readings.map((item, index) => [30 + index * 99, 112 - item.count / top * 96]);
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
  const current = exercise.id === workout.active.exercise_id;
  document.getElementById('exercise-state').textContent = current ? 'Next set' : 'Exercise preview';
  document.getElementById('exercise-name').textContent = exercise.name;
  document.getElementById('set-position').textContent = current ? `Set ${workout.active.set_number} / ${workout.active.working_sets}` : `${exercise.working_sets} sets`;
  const image = document.getElementById('exercise-image');
  image.src = exercise.svg_url;
  image.alt = `${exercise.name} illustration`;
  document.getElementById('weight').textContent = current ? workout.active.target_weight : exercise.target_weight;
  document.getElementById('reps').textContent = `${exercise.rep_min}–${exercise.rep_max}`;
  document.getElementById('rest').hidden = !current;
  document.querySelector('.workout-footer').hidden = !current;
  document.querySelectorAll('.exercise').forEach(button => {
    const selected = button.dataset.exercise === exercise.id;
    button.classList.toggle('selected', selected);
    button.setAttribute('aria-pressed', String(selected));
  });
}

function render(data) {
  if (data.demo !== true || data.live_database_accessed !== false) throw new Error('Only demo data supported');
  document.getElementById('week-label').textContent = data.week;
  document.getElementById('session-count').textContent = `${data.sessions.length} sessions`;
  const week = document.getElementById('week');
  ['Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat', 'Sun'].forEach((day, index) => {
    const column = element('div', 'day');
    const heading = element('div', 'day-heading');
    heading.append(element('span', '', day), element('strong', '', 12 + index));
    const session = data.sessions.find(item => item.day === day);
    const slot = element('div', `slot ${session ? 'training' : 'recovery'}${session?.status === 'STARTED' ? ' current' : ''}`);
    if (session) {
      slot.append(element('b', '', session.start), element('span', '', 'Full body'));
      slot.title = `Prep ${session.prep}, leave ${session.leave}, finish ${session.end}`;
    } else {
      slot.textContent = '·';
      slot.setAttribute('aria-label', `${day}, recovery day`);
    }
    column.append(heading, slot);
    week.append(column);
  });
  const latest = data.crowd.readings.at(-1);
  document.getElementById('attendance-count').textContent = latest.count;
  document.getElementById('fetched-at').textContent = `Fetched ${latest.time}`;
  document.getElementById('cadence').textContent = `${data.crowd.poll_every_minutes} min`;
  attendanceChart(data.crowd.readings);
  const workout = data.workout;
  document.getElementById('eta').textContent = workout.eta;
  document.getElementById('rest-duration').textContent = `${workout.rest_seconds}s`;
  const queue = document.getElementById('queue');
  workout.exercises.forEach(exercise => {
    const button = element('button', 'exercise');
    button.dataset.exercise = exercise.id;
    button.setAttribute('aria-label', `Preview ${exercise.name}`);
    const image = element('img');
    image.src = exercise.svg_url;
    image.alt = '';
    button.append(image, element('span', '', exercise.name));
    button.addEventListener('click', () => showExercise(exercise, workout));
    queue.append(button);
  });
  showExercise(workout.exercises.find(exercise => exercise.id === workout.active.exercise_id), workout);
}

for (const source of ['api', 'google']) {
  document.getElementById(`${source}-source`).addEventListener('click', () => {
    for (const view of ['api', 'google']) {
      const selected = view === source;
      document.getElementById(`${view}-view`).hidden = !selected;
      const button = document.getElementById(`${view}-source`);
      button.classList.toggle('selected', selected);
      button.setAttribute('aria-pressed', String(selected));
    }
  });
}

fetch('/api/demo')
  .then(response => {
    if (!response.ok) throw new Error('Demo request failed');
    return response.json();
  })
  .then(render)
  .catch(() => { document.getElementById('load-error').hidden = false; });
