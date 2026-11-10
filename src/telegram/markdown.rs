//! The Markdown subset coach cards and replies use, as Telegram HTML: `**bold**`, `*italic*`,
//! `~~strike~~`, `` `code` ``, `> quote` lines and `>> collapsed quote` lines (tap to expand).
//! Everything else is escaped text, so stray `<` or `_` never break a send.
use std::sync::LazyLock;

use regex::Regex;

static CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`([^`\n]+)`").unwrap());
static BOLD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\*\*([^*\n]+?)\*\*").unwrap());
static STRIKE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"~~([^~\n]+?)~~").unwrap());
// Single-star italic only around non-space text, so "* item" bullets and "3 * 4" stay literal.
static ITALIC: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(^|[^\w*])\*([^*\s][^*\n]*?[^*\s]|[^*\s])\*([^\w*]|$)").unwrap());

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn inline(text: &str) -> String {
    let bold = BOLD.replace_all(&escape(text), "<b>$1</b>").into_owned();
    let strike = STRIKE.replace_all(&bold, "<s>$1</s>").into_owned();
    ITALIC.replace_all(&strike, "$1<i>$2</i>$3").into_owned()
}

fn line(text: &str) -> String {
    let mut out = String::new();
    let mut last = 0;
    for m in CODE.captures_iter(text) {
        let whole = m.get(0).unwrap();
        out.push_str(&inline(&text[last..whole.start()]));
        out.push_str(&format!("<code>{}</code>", escape(&m[1])));
        last = whole.end();
    }
    out.push_str(&inline(&text[last..]));
    out
}

/// The quote tag a line opens and its text: `>> ` collapsed, `> ` plain, else none.
fn quoted(text: &str) -> (Option<&'static str>, &str) {
    if let Some(rest) = text.strip_prefix(">> ") {
        (Some("<blockquote expandable>"), rest)
    } else if let Some(rest) = text.strip_prefix("> ") {
        (Some("<blockquote>"), rest)
    } else {
        (None, text)
    }
}

pub fn to_html(text: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut open: Option<&str> = None;
    for raw in text.split('\n') {
        let (tag, body) = quoted(raw);
        if open.is_some() && open != tag {
            out.last_mut().expect("open quote has a line").push_str("</blockquote>");
            open = None;
        }
        let mut html = line(body);
        if open.is_none()
            && let Some(tag) = tag
        {
            html = format!("{tag}{html}");
            open = Some(tag);
        }
        out.push(html);
    }
    if open.is_some() {
        out.last_mut().expect("open quote has a line").push_str("</blockquote>");
    }
    out.join("\n")
}

/// Fallback when Telegram rejects the HTML: the text with markers stripped.
pub fn plain(text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').map(|l| quoted(l).1).collect();
    let text = CODE.replace_all(&lines.join("\n"), "$1").into_owned();
    let text = STRIKE.replace_all(&text, "$1").into_owned();
    let text = BOLD.replace_all(&text, "$1").into_owned();
    ITALIC.replace_all(&text, "$1$2$3").into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_markdown() {
        assert_eq!(to_html("⏱ 1:25 until **Bench** set 2/2"), "⏱ 1:25 until <b>Bench</b> set 2/2");
        assert_eq!(to_html("✍️ **Reply reps × weight:** `10x40`"), "✍️ <b>Reply reps × weight:</b> <code>10x40</code>");
        assert_eq!(to_html("* a <b> & 3 * 4 *ok*"), "* a &lt;b&gt; &amp; 3 * 4 <i>ok</i>");
        assert_eq!(to_html("`**x**`"), "<code>**x**</code>");
    }

    #[test]
    fn quotes_and_strike() {
        let text = "**Done**\n>> Bench 8 × 85 kg\n>> ~~Dips~~ skipped\n> a 3 > 2 note\n*after*";
        assert_eq!(
            to_html(text),
            "<b>Done</b>\n<blockquote expandable>Bench 8 × 85 kg\n<s>Dips</s> skipped</blockquote>\n<blockquote>a 3 &gt; 2 note</blockquote>\n<i>after</i>"
        );
        assert_eq!(plain(text), "Done\nBench 8 × 85 kg\nDips skipped\na 3 > 2 note\nafter");
        assert_eq!(to_html(">no space"), "&gt;no space");
    }
}
