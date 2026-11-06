//! Template change proposals: the agent suggests edits, the owner accepts or declines each line, and
//! only an accepted line touches the template (through `templates::edit_exercise`, the same path as
//! `template_edit_exercise`). Declined lines keep that exercise out of new proposals for a while.
use serde_json::{Map, Value, json};

use crate::db::models::{ProposalStatus, TemplateProposal};
use crate::db::{Connection, Json, new_id};
use crate::domain::core::mutate;
use crate::domain::templates::{ExerciseEdit, edit_exercise, get_template};
use crate::domain::view::Button;
use crate::domain::weekly::refresh_plans;
use crate::error::Result;
use crate::time::Ts;

/// A declined exercise isn't proposed again for this long.
const DECLINE_QUIET_DAYS: i64 = 28;

pub struct NewLine {
    pub exercise: String,
    pub changes: Map<String, Value>,
    pub position: Option<usize>,
    pub summary: String,
}

/// What the owner sees: the message text and one accept/decline row per undecided line.
#[derive(Debug)]
pub struct Rendered {
    pub text: String,
    pub buttons: Vec<Vec<Button>>,
}

#[derive(Debug)]
pub struct Created {
    pub batch_id: String,
    pub rendered: Rendered,
    /// Lines left out, with the reason.
    pub skipped: Vec<(String, String)>,
}

/// Apply the edit and roll it back: the proposal must be applicable as stored, or it's rejected now
/// instead of when the owner taps.
fn dry_run(db: &Connection, template_id: &str, line: &NewLine) -> Result<()> {
    db.execute_batch("SAVEPOINT proposal_check")?;
    let result = edit_exercise(db, template_id, &line.exercise, ExerciseEdit::Change(&line.changes), line.position);
    db.execute_batch("ROLLBACK TO proposal_check; RELEASE proposal_check")?;
    result.map(|_| ())
}

fn blocked(db: &Connection, template_id: &str, exercise: &str, now: Ts) -> Result<Option<&'static str>> {
    let since = now.mins(-DECLINE_QUIET_DAYS * 24 * 60);
    let hit = TemplateProposal::first(
        db,
        "WHERE template_id = ?1 AND exercise = ?2 AND (status = 'PENDING' OR (status = 'DECLINED' AND decided_at > ?3))",
        rusqlite::params![template_id, exercise, since],
    )?;
    Ok(hit.map(|p| if p.status == ProposalStatus::Pending { "already proposed, waiting for the owner" } else { "owner declined this recently" }))
}

/// Store the valid, not-recently-declined lines as one batch. Errors if none are left to send.
pub fn create(db: &Connection, template_id: &str, lines: Vec<NewLine>, now: Ts) -> Result<Created> {
    let template = get_template(db, template_id)?;
    let batch_id = new_id();
    let mut skipped = vec![];
    for line in lines {
        let Some(spec) = template.exercises.iter().find(|e| [&e.id, &e.name].iter().any(|v| v.eq_ignore_ascii_case(&line.exercise))) else {
            skipped.push((line.exercise, "not in the template".into()));
            continue;
        };
        if let Some(reason) = blocked(db, template_id, &spec.id, now)? {
            skipped.push((line.exercise, reason.into()));
            continue;
        }
        let line = NewLine { exercise: spec.id.clone(), ..line };
        if let Err(e) = dry_run(db, template_id, &line) {
            skipped.push((line.exercise, e.to_string()));
            continue;
        }
        TemplateProposal {
            id: new_id(), batch_id: batch_id.clone(), template_id: template_id.into(), exercise: line.exercise, changes_json: Json(line.changes),
            position: line.position.map(|p| p as i64), summary: line.summary, status: ProposalStatus::Pending, created_at: now, decided_at: None,
        }
        .insert(db)?;
    }
    let rendered = render(db, &batch_id)?;
    if rendered.buttons.is_empty() {
        let reasons: Vec<String> = skipped.iter().map(|(exercise, reason)| format!("{exercise}: {reason}")).collect();
        crate::bail!("NOTHING_TO_PROPOSE", "No proposal sent ({})", reasons.join("; "));
    }
    Ok(Created { batch_id, rendered, skipped })
}

fn batch(db: &Connection, batch_id: &str) -> Result<Vec<TemplateProposal>> {
    TemplateProposal::select(db, "WHERE batch_id = ?1 ORDER BY rowid", [batch_id])
}

/// The proposal message as it stands: decided lines show their outcome, the rest keep their buttons.
pub fn render(db: &Connection, batch_id: &str) -> Result<Rendered> {
    let rows = batch(db, batch_id)?;
    let mut text = String::from("Template changes. Nothing changes until you accept a line.");
    let mut buttons = vec![];
    for (i, row) in rows.iter().enumerate() {
        let mark = match row.status {
            ProposalStatus::Pending => "▫️",
            ProposalStatus::Accepted => "✅",
            ProposalStatus::Declined => "✖️",
        };
        text.push_str(&format!("\n{}. {mark} {}", i + 1, row.summary));
        if row.status == ProposalStatus::Pending {
            let id = &row.id[..8];
            buttons.push(vec![
                Button { text: format!("✅ {}", i + 1), data: format!("tp:a:{id}") },
                Button { text: format!("✖️ {}", i + 1), data: format!("tp:d:{id}") },
            ]);
        }
    }
    Ok(Rendered { text, buttons })
}

pub struct Decision {
    pub toast: String,
    pub applied: bool,
    pub summary: String,
    pub rendered: Rendered,
}

/// A `tp:<a|d>:<id>` tap. A line already decided is left alone (double taps, old buttons), and
/// retries with the same request ID return the first result.
pub fn decide(db: &Connection, payload: &str, now: Ts, request_id: &str) -> Result<Decision> {
    let mut parts = payload.strip_prefix("tp:").unwrap_or(payload).split(':');
    let (accept, id) = (parts.next() == Some("a"), parts.next().unwrap_or_default());
    let Some(row) = TemplateProposal::first(db, "WHERE substr(id, 1, length(?1)) = ?1", [id])?.filter(|_| !id.is_empty()) else {
        crate::bail!("PROPOSAL_UNKNOWN", "Old button.");
    };
    let operation = if accept { "proposal.accept" } else { "proposal.decline" };
    let result = mutate(db, operation, request_id, json!({"id": row.id}), now, || {
        let mut row = TemplateProposal::get(db, &row.id)?.expect("loaded above");
        if row.status != ProposalStatus::Pending {
            return Ok(json!({"applied": false, "toast": "Already decided."}));
        }
        if accept {
            edit_exercise(db, &row.template_id, &row.exercise, ExerciseEdit::Change(&row.changes_json), row.position.map(|p| p as usize))?;
            refresh_plans(db, now)?;
        }
        row.status = if accept { ProposalStatus::Accepted } else { ProposalStatus::Declined };
        row.decided_at = Some(now);
        row.update(db)?;
        Ok(json!({"applied": accept, "toast": if accept { "Applied." } else { "Declined." }}))
    })?;
    Ok(Decision {
        toast: result["data"]["toast"].as_str().unwrap_or_default().into(),
        applied: result["data"]["applied"].as_bool().unwrap_or(false),
        summary: row.summary.clone(),
        rendered: render(db, &row.batch_id)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::templates::import_template;
    use crate::domain::workout::tests::db;

    fn setup() -> Connection {
        let db = db();
        import_template(&db, &serde_json::from_str(&std::fs::read_to_string("config/exercises.seed.json").unwrap()).unwrap()).unwrap();
        db
    }

    fn line(exercise: &str, changes: Value) -> NewLine {
        NewLine { exercise: exercise.into(), changes: changes.as_object().unwrap().clone(), position: None, summary: format!("{exercise} change") }
    }

    fn tap(created: &Created, row: usize, action: &str) -> String {
        let data = &created.rendered.buttons[row][if action == "a" { 0 } else { 1 }].data;
        assert!(data.starts_with(&format!("tp:{action}:")));
        data.clone()
    }

    #[test]
    fn nothing_changes_until_accepted_and_only_the_proposed_fields() {
        let db = setup();
        let before = get_template(&db, "upper-a").unwrap();
        let created = create(&db, "upper-a", vec![line("bench", json!({"target_weight": 82.5, "working_sets": 3, "rep_max": 8}))], Ts::now()).unwrap();
        assert_eq!(get_template(&db, "upper-a").unwrap(), before);
        let decision = decide(&db, &tap(&created, 0, "a"), Ts::now(), "r1").unwrap();
        assert!(decision.applied);
        let mut expected = before.clone();
        let bench = expected.exercises.iter_mut().find(|e| e.id == "bench").unwrap();
        (bench.target_weight, bench.working_sets, bench.rep_max) = (82.5, 3, 8);
        assert_eq!(get_template(&db, "upper-a").unwrap(), expected);
    }

    #[test]
    fn accept_is_idempotent_across_retries_and_double_taps() {
        let db = setup();
        let created = create(&db, "upper-a", vec![line("bench", json!({"target_weight": 82.5}))], Ts::now()).unwrap();
        let data = tap(&created, 0, "a");
        assert!(decide(&db, &data, Ts::now(), "r1").unwrap().applied);
        // Same callback redelivered.
        assert!(decide(&db, &data, Ts::now(), "r1").unwrap().applied);
        // A second tap on the same button (new callback): no second application, and a later template
        // edit by the owner is not overwritten.
        edit_exercise(&db, "upper-a", "bench", ExerciseEdit::Change(json!({"target_weight": 90}).as_object().unwrap()), None).unwrap();
        let again = decide(&db, &data, Ts::now(), "r2").unwrap();
        assert!(!again.applied);
        assert_eq!(get_template(&db, "upper-a").unwrap().spec("bench").unwrap().target_weight, 90.0);
    }

    #[test]
    fn declined_exercise_is_not_reproposed_until_the_quiet_period_ends() {
        let db = setup();
        let now = Ts::parse("2026-10-12T18:00:00+00:00", None).unwrap();
        let created = create(&db, "upper-a", vec![line("bench", json!({"target_weight": 82.5})), line("row", json!({"target_weight": 65}))], now).unwrap();
        decide(&db, &tap(&created, 0, "d"), now, "r1").unwrap();
        // Bench (declined) is skipped, with a different change too; row is still pending.
        let err = create(&db, "upper-a", vec![line("bench", json!({"working_sets": 4})), line("row", json!({"target_weight": 70}))], now.mins(60)).unwrap_err();
        assert_eq!(err.code(), "NOTHING_TO_PROPOSE");
        let later = now.mins((DECLINE_QUIET_DAYS + 1) * 24 * 60);
        assert_eq!(create(&db, "upper-a", vec![line("bench", json!({"working_sets": 4}))], later).unwrap().skipped, vec![]);
    }

    #[test]
    fn unappliable_lines_are_rejected_at_creation_and_leave_no_trace() {
        let db = setup();
        let before = get_template(&db, "upper-a").unwrap();
        let created = create(&db, "upper-a", vec![line("bench", json!({"rep_min": 12, "rep_max": 8})), line("squat", json!({})), line("row", json!({"target_weight": 65}))], Ts::now()).unwrap();
        assert_eq!(created.skipped.len(), 2);
        assert_eq!(created.rendered.buttons.len(), 1);
        assert_eq!(get_template(&db, "upper-a").unwrap(), before);
    }
}
