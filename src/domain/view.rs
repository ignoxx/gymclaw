//! What the coach shows in Telegram. Produced by the training domain, rendered by `telegram::coach`.
use serde::{Deserialize, Serialize};

use crate::time::Ts;

/// Inline button whose callback data is `gc:<action>:<ref>` (≤ 64 bytes), handled without the model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Button {
    pub text: String,
    pub data: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CardKind {
    #[default]
    Set,
    /// Countdown; deleted when the next set card arrives.
    Rest,
    /// Swap option; deleted once the owner picks or the workout moves on.
    Option,
    /// Session overview image. The photo goes out alone; `text` is for chat history and the agent.
    Preview,
    #[serde(other)]
    Other,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Card {
    pub text: String,
    /// Local PNG path sent as a photo with `text` as its caption (no caption for previews).
    pub photo: Option<String>,
    pub buttons: Vec<Vec<Button>>,
    /// Rest cards count down to this.
    pub rest_until: Option<Ts>,
    pub kind: CardKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Cleanup {
    /// Delete the live card and extras instead of retiring them with the ack.
    Delete,
}

/// One coach step. Flags mirror the old plugin protocol (see `telegram::coach` for how each renders).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CoachView {
    /// False: not a coach message (e.g. typed text that isn't a set); the agent gets it.
    pub handled: bool,
    /// Reaction on the owner's typed set.
    pub react: Option<String>,
    /// Appended to the retired card ("✅ 10 × 40 kg").
    pub ack: Option<String>,
    #[serde(default)]
    pub cards: Vec<Card>,
    pub cleanup: Option<Cleanup>,
    /// Same message, new content (e.g. rest shortened).
    #[serde(default)]
    pub refresh: bool,
    /// Leave the current cards as they are.
    #[serde(default)]
    pub keep: bool,
    /// Drop swap options and restore the live card's buttons.
    #[serde(default)]
    pub restore: bool,
    /// Cards are their own messages (previews); the live card stays.
    #[serde(default)]
    pub standalone: bool,
    /// The rest that was counting down is over (scheduler drops the fallback ping).
    #[serde(default)]
    pub rest_over: bool,
    /// Swap menu: the live card's buttons become these, `cards` are the options below it.
    pub live_buttons: Option<Vec<Vec<Button>>>,
}
