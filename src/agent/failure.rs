//! What a failed turn says. The owner gets one plain line (`format_failure`); the turn log gets the
//! classified diagnosis (`diagnose`). Raw provider errors, prompts and tool data reach neither.

/// What the turn observed before it failed.
#[derive(Clone, Debug, Default)]
pub struct FailureState {
    /// Our error text (model error, timeout, step limit…), classified below.
    pub error: String,
    /// `finish_reason` of the last model call.
    pub stop: Option<String>,
    pub output: Option<u64>,
    pub reasoning: Option<u64>,
    /// Tools that ran (they may have changed data).
    pub tool_calls: usize,
    /// A model call was made at all.
    pub observed_input: bool,
}

/// Failure class: (code, reason, the model is reachable).
fn classify(state: &FailureState) -> (&'static str, &'static str, bool) {
    let error = state.error.to_lowercase();
    let has = |patterns: &[&str]| patterns.iter().any(|p| error.contains(p));
    if state.stop.as_deref() == Some("length") {
        ("OUTPUT_LIMIT", "Model exhausted output/reasoning budget before finishing its answer.", true)
    } else if error.contains("context") && has(&["exceed", "limit", "overflow", "long"]) || error.contains("too many tokens") {
        ("CONTEXT_LIMIT", "Model request exceeded context capacity.", true)
    } else if has(&["429", "rate limit", "rate-limit", "ratelimit", "quota"]) {
        ("RATE_LIMIT", "Provider rate limit or quota blocked response.", false)
    } else if has(&["401", "403", "unauthoriz", "authentication", "api key"]) {
        ("PROVIDER_AUTH", "Provider rejected authentication or access.", false)
    } else if has(&["timeout", "timed out", "deadline"]) {
        ("TIMEOUT", "Model request exceeded time limit.", false)
    } else if has(&["connection", "upstream", "502", "503", "504"]) {
        ("UPSTREAM_ERROR", "Model provider connection failed.", false)
    } else if has(&["step limit", "repeated the same call"]) {
        ("STUCK", "Model kept calling tools without finishing.", true)
    } else {
        ("NO_ANSWER", "Model returned no usable answer; cause not reported.", true)
    }
}

/// The owner's line: what happened, in plain words. It never claims nothing saved: the fast path may
/// have logged a set before handing the message over, and a resend would log it twice.
pub fn format_failure(state: &FailureState) -> String {
    let (_, _, reachable) = classify(state);
    let what = if reachable { "I got stuck on that." } else { "I can't reach my model right now." };
    format!("⚠️ {what} If it was a change, check whether it saved before sending it again.")
}

/// Classified diagnosis for the turn log ("STUCK: … · output 12 tokens · 2 tool calls").
pub fn diagnose(state: &FailureState) -> String {
    let (code, reason, _) = classify(state);
    let mut metrics = vec![];
    if let Some(o) = state.output {
        metrics.push(format!("output {} tokens", thousands(o)));
    }
    if let Some(r) = state.reasoning {
        metrics.push(format!("reasoning {}", thousands(r)));
    }
    metrics.push(format!("{} tool calls", state.tool_calls));
    if !state.observed_input {
        metrics.push("no model call".into());
    }
    format!("{code}: {reason} {}", metrics.join(" · "))
}

fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_line_is_plain_and_the_log_keeps_the_class() {
        let state = FailureState { stop: Some("length".into()), output: Some(16384), reasoning: Some(15900), tool_calls: 2, observed_input: true, ..Default::default() };
        assert_eq!(format_failure(&state), "⚠️ I got stuck on that. If it was a change, check whether it saved before sending it again.");
        assert_eq!(diagnose(&state), "OUTPUT_LIMIT: Model exhausted output/reasoning budget before finishing its answer. output 16,384 tokens · reasoning 15,900 · 2 tool calls");
        let state = FailureState { error: "MODEL_ERROR: 429 Provider returned error sk-secret".into(), observed_input: true, ..Default::default() };
        assert_eq!(format_failure(&state), "⚠️ I can't reach my model right now. If it was a change, check whether it saved before sending it again.");
        assert!(diagnose(&state).starts_with("RATE_LIMIT:") && !diagnose(&state).contains("sk-secret"));
    }
}
