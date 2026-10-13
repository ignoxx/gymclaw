//! The Markdown subset coach cards and replies use, as Telegram HTML: `**bold**`, `*italic*`,
//! `` `code` ``. Everything else is escaped text, so stray `<` or `_` never break a send.
use std::sync::LazyLock;

use regex::Regex;

static CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`([^`\n]+)`").unwrap());
static BOLD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\*\*([^*\n]+?)\*\*").unwrap());
// Single-star italic only around non-space text, so "* item" bullets and "3 * 4" stay literal.
static ITALIC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(^|[^\w*])\*([^*\s][^*\n]*?[^*\s]|[^*\s])\*([^\w*]|$)").unwrap());

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn inline(text: &str) -> String {
    let bold = BOLD.replace_all(&escape(text), "<b>$1</b>").into_owned();
    ITALIC.replace_all(&bold, "$1<i>$2</i>$3").into_owned()
}

pub fn to_html(text: &str) -> String {
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

/// Fallback when Telegram rejects the HTML: the text with markers stripped.
pub fn plain(text: &str) -> String {
    let text = CODE.replace_all(text, "$1");
    BOLD.replace_all(&text, "$1").into_owned()
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
}
