/* Read-only demo UI. All text uses textContent; no external services or timers. */
function element(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function render(data) {
  if (data.demo !== true || data.live_database_accessed !== false) {
    throw new Error('Only isolated demo snapshots are supported');
  }
  document.getElementById('week-label').textContent = data.week;
  document.getElementById('sessions-stat').textContent = `${data.sessions.length} / ${data.target_sessions}`;
  const grid = document.getElementById('week-grid');
  ['Mon', 'Tue', 'Wed', 'Thu', 'Fri'].forEach((day, index) => {
    const column = element('div', 'day');
    const label = element('div', 'day-label');
    label.append(element('strong', '', day), element('span', '', `${12 + index} OCT`));
    column.append(label);
    const session = data.sessions.find(item => item.day === day);
    if (session) {
      const slot = element('div', `training-slot${session.status === 'STARTED' ? ' active' : ''}`);
      slot.append(element('b', '', session.start), element('strong', '', session.template),
        element('span', '', `${session.start}–${session.end}`),
        element('span', '', session.status === 'STARTED' ? 'Started · demo' : 'Tentative'));
      column.append(slot);
    } else {
      column.append(element('div', 'rest-day', 'Recovery'));
    }
    grid.append(column);
  });
  const workout = data.workout;
  document.getElementById('exercise-name').textContent = workout.active.name;
  document.getElementById('set-position').textContent = `Set ${workout.active.set_number} / ${workout.active.working_sets}`;
  document.getElementById('target-weight').textContent = workout.active.target_weight;
  document.getElementById('target-reps').textContent = `${workout.active.rep_min}–${workout.active.rep_max}`;
  document.getElementById('eta').textContent = workout.eta;
  const exerciseList = document.getElementById('exercise-list');
  workout.exercises.forEach(exercise => {
    exerciseList.append(element('span', `exercise-tag${exercise.id === workout.active.exercise_id ? ' active' : ''}`, exercise.name));
  });
  const chart = document.getElementById('crowd-chart');
  const max = Math.max(...data.crowd.readings.map(reading => reading.count), 1);
  data.crowd.readings.forEach(reading => {
    const column = element('div', 'bar-column');
    const bar = element('div', 'bar');
    bar.style.height = `${Math.round(reading.count / max * 92)}px`;
    bar.setAttribute('aria-label', `${reading.count} synthetic reported check-ins at ${reading.time}`);
    column.append(element('span', 'bar-value', reading.count), bar, element('span', 'bar-time', reading.time));
    chart.append(column);
  });
}

document.querySelectorAll('.tab').forEach(tab => {
  tab.addEventListener('click', () => {
    document.querySelectorAll('.tab').forEach(other => {
      const selected = other === tab;
      other.classList.toggle('selected', selected);
      other.setAttribute('aria-pressed', String(selected));
    });
    document.querySelectorAll('[data-view]').forEach(card => {
      card.hidden = !card.dataset.view.split(' ').includes(tab.dataset.tab);
    });
  });
});

fetch('/api/demo')
  .then(response => {
    if (!response.ok) throw new Error('Demo request failed');
    return response.json();
  })
  .then(render)
  .catch(() => { document.getElementById('loading-error').hidden = false; });
