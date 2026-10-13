//! One sanitized line when a turn fails (port of the `failure-details` plugin). Raw provider errors,
//! prompts and tool data never reach the owner.

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
    pub run_id: String,
}

pub fn format_failure(state: &FailureState) -> String {
    let error = state.error.to_lowercase();
    let has = |patterns: &[&str]| patterns.iter().any(|p| error.contains(p));
    let (code, reason) = if state.stop.as_deref() == Some("length") {
        ("OUTPUT_LIMIT", "Model exhausted output/reasoning budget before finishing its answer.")
    } else if error.contains("context") && has(&["exceed", "limit", "overflow", "long"]) || error.contains("too many tokens") {
        ("CONTEXT_LIMIT", "Model request exceeded context capacity.")
    } else if has(&["429", "rate limit", "rate-limit", "ratelimit", "quota"]) {
        ("RATE_LIMIT", "Provider rate limit or quota blocked response.")
    } else if has(&["401", "403", "unauthoriz", "authentication", "api key"]) {
        ("PROVIDER_AUTH", "Provider rejected authentication or access.")
    } else if has(&["timeout", "timed out", "deadline"]) {
        ("TIMEOUT", "Model request exceeded time limit.")
    } else if has(&["connection", "upstream", "502", "503", "504"]) {
        ("UPSTREAM_ERROR", "Model provider connection failed.")
    } else if has(&["step limit", "repeated the same call"]) {
        ("STUCK", "Model kept calling tools without finishing.")
    } else {
        ("NO_ANSWER", "Model returned no usable answer; cause not reported.")
    };
    let mut metrics = vec![];
    if let Some(o) = state.output {
        metrics.push(format!("output {} tokens", thousands(o)));
    }
    if let Some(r) = state.reasoning {
        metrics.push(format!("reasoning {}", thousands(r)));
    }
    let safety = if state.tool_calls > 0 {
        "Tool actions may already have run. Check saved state before retrying."
    } else if state.observed_input {
        "No tool calls observed in this run. No automatic retry performed."
    } else {
        "Action status unknown. Check saved state before retrying."
    };
    let reference = if state.run_id.len() >= 8 { format!(" Ref: {}.", &state.run_id[..8]) } else { String::new() };
    let metrics = if metrics.is_empty() { String::new() } else { format!("\n{}.", metrics.join(" · ")) };
    format!("⚠️ {code}: {reason}{metrics}\n{safety}{reference}")
}

fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
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
    fn classifies_without_leaking() {
        let state = FailureState { stop: Some("length".into()), output: Some(16384), reasoning: Some(15900), tool_calls: 2, observed_input: true, run_id: "1b2c3d4e-0000-0000-0000-000000000000".into(), ..Default::default() };
        assert_eq!(
            format_failure(&state),
            "⚠️ OUTPUT_LIMIT: Model exhausted output/reasoning budget before finishing its answer.\noutput 16,384 tokens · reasoning 15,900.\nTool actions may already have run. Check saved state before retrying. Ref: 1b2c3d4e."
        );
        let state = FailureState { error: "MODEL_ERROR: 429 Provider returned error sk-secret".into(), observed_input: true, ..Default::default() };
        assert_eq!(format_failure(&state), "⚠️ RATE_LIMIT: Provider rate limit or quota blocked response.\nNo tool calls observed in this run. No automatic retry performed.");
    }
}
