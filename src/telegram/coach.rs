//! Renders `CoachView`s in the owner chat (port of the `gymclaw-coach` plugin's `createCoach`).
//!
//! Keeps one "live" card (current set or rest countdown) and "extras" (swap options). Old buttons
//! disappear as soon as the workout moves on. Rest cards are edited every 5 s and, exactly at
//! `rest_until` (+50 ms, so the domain sees the rest as due), the countdown is deleted and a fresh
//! set card is sent so the phone notifies.
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Weak};
use std::time::Duration;

use tokio::sync::Mutex;

use super::{Keyboard, Telegram, is_gone};
use crate::domain::view::{Card, CardKind, Cleanup, CoachView};
use crate::error::Result;
use crate::time::Ts;

pub const TICK: Duration = Duration::from_secs(5);

pub type Clock = Arc<dyn Fn() -> Ts + Send + Sync>;
/// Called when a countdown reaches zero: `coach::handle_rest_over`, None on failure.
pub type RestOver = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Option<CoachView>> + Send>> + Send + Sync>;
/// Called with the text and message id of every card sent (for `chat_message`).
pub type Record = Arc<dyn Fn(&str, i64) + Send + Sync>;

/// "⏱ 1:25 until Bench set 2/2" for rest cards; other cards as they are.
pub fn with_rest(card: &Card, now: Ts) -> String {
    let Some(until) = card.rest_until else { return card.text.clone() };
    let ms = (until - now).as_millis().max(0);
    let left = (ms + 999) / 1000;
    format!("⏱ {}:{:02} {}", left / 60, left % 60, card.text)
}

/// The message a button was tapped on. `text` is its visible text, for messages from before a restart.
#[derive(Clone, Debug, Default)]
pub struct Tapped {
    pub message_id: i64,
    pub text: Option<String>,
}

#[derive(Clone, Debug)]
struct Entry {
    message_id: i64,
    card: Card,
    /// Identity of the running countdown (0 = none). A step whose id no longer matches stops.
    countdown: u64,
}

impl Entry {
    /// Countdowns and swap options are throwaway: deleted once replaced, so the chat stays clean.
    fn throwaway(&self) -> bool {
        matches!(self.card.kind, CardKind::Rest | CardKind::Option)
    }
}

#[derive(Default)]
struct State {
    live: Option<Entry>,
    extras: Vec<Entry>,
    next_countdown: u64,
}

pub struct Coach {
    tg: Arc<dyn Telegram>,
    chat: i64,
    clock: Clock,
    rest_over: Option<RestOver>,
    record: Option<Record>,
    state: Mutex<State>,
    me: Weak<Coach>,
}

fn keyboard(card: &Card) -> &Keyboard {
    &card.buttons
}

impl Coach {
    pub fn new(tg: Arc<dyn Telegram>, chat: i64, clock: Clock, rest_over: Option<RestOver>, record: Option<Record>) -> Arc<Coach> {
        Arc::new_cyclic(|me| Coach { tg, chat, clock, rest_over, record, state: Mutex::new(State::default()), me: me.clone() })
    }

    fn now(&self) -> Ts {
        (self.clock)()
    }

    /// "message is not modified" and already-deleted messages are expected.
    async fn quietly(&self, result: Result<()>) {
        if let Err(e) = result {
            tracing::debug!("coach edit skipped: {e}");
        }
    }

    async fn send(&self, text: &str, card: &Card) -> Result<i64> {
        let caption = if card.kind == CardKind::Preview { "" } else { text };
        let id = self.tg.send(self.chat, caption, card.photo.as_deref().map(Path::new), keyboard(card)).await?;
        if let Some(record) = &self.record {
            record(text, id);
        }
        Ok(id)
    }

    /// Photo cards are edited as captions; a text edit on them fails first and costs a round trip.
    async fn edit(&self, entry: &Entry, text: &str, buttons: &Keyboard) -> Result<()> {
        self.tg.edit(self.chat, entry.message_id, text, buttons, entry.card.photo.is_some()).await
    }

    async fn remove(&self, entry: &Entry) {
        self.quietly(self.tg.remove(self.chat, entry.message_id).await).await;
    }

    async fn retire(&self, entry: &Entry, ack: Option<&str>) {
        if entry.throwaway() {
            return self.remove(entry).await;
        }
        let text = match ack {
            Some(ack) => format!("{}\n{ack}", entry.card.text),
            None => entry.card.text.clone(),
        };
        self.quietly(self.edit(entry, &text, &vec![]).await).await;
    }

    async fn clear_extras(&self, state: &mut State) {
        for entry in std::mem::take(&mut state.extras) {
            self.remove(&entry).await;
        }
    }

    /// Restart the live card's countdown (its rest_until may have changed).
    fn start_countdown(&self, state: &mut State) {
        let Some(live) = state.live.as_mut() else { return };
        state.next_countdown += 1;
        live.countdown = state.next_countdown;
        let id = live.countdown;
        let Some(me) = self.me.upgrade() else { return };
        tokio::spawn(async move { me.countdown(id).await });
    }

    async fn countdown(self: Arc<Self>, id: u64) {
        loop {
            let until = {
                let state = self.state.lock().await;
                match &state.live {
                    Some(live) if live.countdown == id => live.card.rest_until,
                    _ => return,
                }
            };
            let Some(until) = until else { return };
            let wait = (until - self.now()).as_millis() + 50;
            tokio::time::sleep(Duration::from_millis(wait.clamp(0, TICK.as_millis() as i128) as u64)).await;
            let mut state = self.state.lock().await;
            let Some(live) = state.live.clone().filter(|l| l.countdown == id) else { return };
            if live.card.rest_until.is_some_and(|u| u > self.now()) {
                if let Err(e) = self.edit(&live, &with_rest(&live.card, self.now()), keyboard(&live.card)).await {
                    if is_gone(&e) {
                        // The countdown is gone, so is its job: stop, and never announce the rest it was showing.
                        state.live = None;
                        return;
                    }
                    tracing::debug!("coach countdown edit skipped: {e}");
                }
                continue;
            }
            drop(state);
            // A fresh set card (not an edit) so the phone notifies; the countdown message is deleted.
            let Some(rest_over) = &self.rest_over else { return };
            if let Some(view) = rest_over().await
                && !view.cards.is_empty()
                && let Err(e) = self.apply(&view, None, None).await
            {
                tracing::warn!("coach: next card after rest failed: {e}");
            }
            return;
        }
    }

    /// Apply one coach result. `tapped` is the pressed message; `inbound` a typed set to react to.
    pub async fn apply(&self, view: &CoachView, tapped: Option<&Tapped>, inbound: Option<i64>) -> Result<()> {
        let mut state = self.state.lock().await;
        let now = self.now();
        if let (Some(message), Some(emoji)) = (inbound, &view.react) {
            self.quietly(self.tg.react(self.chat, message, Some(emoji)).await).await;
        }
        if view.standalone {
            // Previews etc. are their own messages; the live workout card stays as it is.
            for card in &view.cards {
                self.send(&card.text, card).await?;
            }
            return Ok(());
        }
        if let Some(buttons) = &view.live_buttons {
            // Swap: the current card's buttons become wait/later, options go below it.
            if let Some(live) = state.live.clone() {
                self.quietly(self.edit(&live, &with_rest(&live.card, now), buttons).await).await;
            }
            self.clear_extras(&mut state).await;
            for card in &view.cards {
                let message_id = self.send(&card.text, card).await?;
                state.extras.push(Entry { message_id, card: card.clone(), countdown: 0 });
            }
            return Ok(());
        }
        if view.refresh && state.live.is_some() && !view.cards.is_empty() {
            // Same message, new content (rest shortened): the running countdown picks up the new time.
            let live = state.live.as_mut().expect("checked");
            live.card = view.cards[0].clone();
            let live = live.clone();
            self.quietly(self.edit(&live, &with_rest(&live.card, now), keyboard(&live.card)).await).await;
            if live.card.rest_until.is_some() {
                self.start_countdown(&mut state);
            }
            return Ok(());
        }
        if view.keep && view.cards.is_empty() {
            return Ok(());
        }
        if view.restore {
            self.clear_extras(&mut state).await;
            if let Some(live) = state.live.clone() {
                self.quietly(self.edit(&live, &with_rest(&live.card, now), keyboard(&live.card)).await).await;
            }
            return Ok(());
        }
        let entries: Vec<Entry> = state.live.iter().chain(state.extras.iter()).cloned().collect();
        let known = tapped.and_then(|t| entries.iter().find(|e| e.message_id == t.message_id)).cloned();
        if view.cleanup == Some(Cleanup::Delete) {
            for entry in &entries {
                self.remove(entry).await;
            }
            if let (Some(t), None) = (tapped, &known) {
                self.quietly(self.tg.remove(self.chat, t.message_id).await).await;
            }
        } else {
            let target = known.clone().or_else(|| if tapped.is_some() { None } else { state.live.clone() });
            match (&target, tapped, &view.ack) {
                (Some(target), _, ack) => self.retire(target, ack.as_deref()).await,
                (None, Some(t), Some(ack)) => {
                    // Message from before a restart: we only know its visible text.
                    let text = format!("{}\n{ack}", t.text.as_deref().unwrap_or(""));
                    self.quietly(self.tg.edit(self.chat, t.message_id, &text, &vec![], false).await).await;
                }
                _ => {}
            }
            for entry in entries.iter().filter(|e| target.as_ref().is_none_or(|t| t.message_id != e.message_id)) {
                self.retire(entry, None).await;
            }
        }
        state.live = None;
        state.extras.clear();
        for card in &view.cards {
            let message_id = self.send(&with_rest(card, self.now()), card).await?;
            state.live = Some(Entry { message_id, card: card.clone(), countdown: 0 });
            if card.rest_until.is_some() {
                self.start_countdown(&mut state);
            }
        }
        Ok(())
    }

    /// A one-off warning that leaves the workout cards alone.
    pub async fn warn(&self, text: &str) {
        let card = Card { text: format!("⚠️ {text}"), ..Card::default() };
        if let Err(e) = self.apply(&CoachView { handled: true, standalone: true, cards: vec![card], ..CoachView::default() }, None, None).await {
            tracing::warn!("coach warning not sent: {e}");
        }
    }

    /// Text of the live card and options, for tests and `/status`.
    pub async fn shown(&self) -> (Option<String>, usize) {
        let state = self.state.lock().await;
        (state.live.as_ref().map(|l| l.card.text.clone()), state.extras.len())
    }
}

#[cfg(test)]
pub mod tests {
    use std::sync::Mutex as StdMutex;

    use async_trait::async_trait;

    use super::*;
    use crate::domain::view::Button;
    use crate::error::Error;
    use crate::telegram::Update;

    #[derive(Clone, Debug, PartialEq)]
    pub enum Call {
        Send(String, Keyboard),
        Edit(i64, String, Keyboard, bool),
        Buttons(i64, Keyboard),
        Remove(i64),
        React(i64, Option<String>),
        Typing,
        Answer(String, Option<String>),
    }

    /// Records calls with the (paused) tokio time they happened at; message ids start at 100.
    pub struct FakeTelegram {
        pub calls: StdMutex<Vec<(u128, Call)>>,
        pub start: tokio::time::Instant,
        pub gone: bool,
        pub fail_send: bool,
    }

    impl FakeTelegram {
        pub fn new() -> Arc<FakeTelegram> {
            Arc::new(FakeTelegram { calls: StdMutex::new(vec![]), start: tokio::time::Instant::now(), gone: false, fail_send: false })
        }
        fn push(&self, call: Call) -> usize {
            let mut calls = self.calls.lock().unwrap();
            calls.push((self.start.elapsed().as_millis(), call));
            calls.iter().filter(|(_, c)| matches!(c, Call::Send(..))).count()
        }
        pub fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().iter().map(|(_, c)| c.clone()).collect()
        }
        pub fn timed(&self) -> Vec<(u128, Call)> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Telegram for FakeTelegram {
        async fn send(&self, _: i64, text: &str, _: Option<&Path>, buttons: &Keyboard) -> Result<i64> {
            if self.fail_send {
                return Err(Error::domain("TELEGRAM_ERROR", "sendMessage: timeout"));
            }
            Ok(99 + self.push(Call::Send(text.into(), buttons.clone())) as i64)
        }
        async fn edit(&self, _: i64, message: i64, text: &str, buttons: &Keyboard, caption: bool) -> Result<()> {
            self.push(Call::Edit(message, text.into(), buttons.clone(), caption));
            if self.gone {
                return Err(Error::domain("TELEGRAM_ERROR", "editMessageText: Bad Request: message to edit not found"));
            }
            Ok(())
        }
        async fn edit_buttons(&self, _: i64, message: i64, buttons: &Keyboard) -> Result<()> {
            self.push(Call::Buttons(message, buttons.clone()));
            Ok(())
        }
        async fn remove(&self, _: i64, message: i64) -> Result<()> {
            self.push(Call::Remove(message));
            Ok(())
        }
        async fn react(&self, _: i64, message: i64, emoji: Option<&str>) -> Result<()> {
            self.push(Call::React(message, emoji.map(String::from)));
            Ok(())
        }
        async fn typing(&self, _: i64) -> Result<()> {
            self.push(Call::Typing);
            Ok(())
        }
        async fn answer_callback(&self, id: &str, text: Option<&str>) -> Result<()> {
            self.push(Call::Answer(id.into(), text.map(String::from)));
            Ok(())
        }
        async fn updates(&self, _: i64, _: u64) -> Result<Vec<Update>> {
            Ok(vec![])
        }
        async fn download(&self, _: &str) -> Result<Vec<u8>> {
            Ok(vec![])
        }
    }

    pub const T0: &str = "2026-10-02T18:00:00Z";

    /// Wall clock that follows paused tokio time from T0.
    pub fn clock() -> Clock {
        let start = tokio::time::Instant::now();
        let t0 = Ts::parse(T0, None).unwrap();
        Arc::new(move || t0 + jiff::SignedDuration::try_from(start.elapsed()).unwrap())
    }

    fn ts(text: &str) -> Ts {
        Ts::parse(text, None).unwrap()
    }

    fn card(text: &str) -> Card {
        Card { text: text.into(), buttons: vec![vec![Button { text: "✅ 10 × 40 kg".into(), data: "gc:log:abcd1234:40:10".into() }]], ..Card::default() }
    }

    fn rest(text: &str, until: &str) -> Card {
        Card { rest_until: Some(ts(until)), kind: CardKind::Rest, ..card(text) }
    }

    fn view(cards: Vec<Card>) -> CoachView {
        CoachView { handled: true, cards, ..CoachView::default() }
    }

    fn next_card(text: &'static str) -> RestOver {
        Arc::new(move || Box::pin(async move { Some(CoachView { cleanup: Some(Cleanup::Delete), ..view(vec![card(text)]) }) }))
    }

    async fn sleep_ms(ms: u64) {
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }

    #[test]
    fn rest_countdown_text() {
        let rest = rest("until **Bench** set 2/2", T0);
        assert_eq!(with_rest(&rest, ts(T0).mins(-1) - jiff::SignedDuration::from_secs(25)), "⏱ 1:25 until **Bench** set 2/2");
        assert_eq!(with_rest(&rest, ts(T0) + jiff::SignedDuration::from_millis(1)), "⏱ 0:00 until **Bench** set 2/2");
    }

    #[tokio::test(start_paused = true)]
    async fn typed_set_then_countdown_then_fresh_card() {
        let tg = FakeTelegram::new();
        let coach = Coach::new(tg.clone(), 1, clock(), Some(next_card("Bench · set 2")), None);
        coach.apply(&view(vec![card("Bench · set 1")]), None, None).await.unwrap();
        let resting = CoachView { react: Some("👍".into()), ack: Some("✅ 10 × 40 kg".into()), ..view(vec![rest("until Bench set 2/2", "2026-10-02T18:01:30Z")]) };
        coach.apply(&resting, None, Some(7)).await.unwrap();
        let calls = tg.calls();
        assert_eq!(calls[1], Call::React(7, Some("👍".into())));
        assert_eq!(calls[2], Call::Edit(100, "Bench · set 1\n✅ 10 × 40 kg".into(), vec![], false));
        assert!(matches!(&calls[3], Call::Send(text, _) if text == "⏱ 1:30 until Bench set 2/2"));
        sleep_ms(5001).await;
        assert!(matches!(tg.calls().last(), Some(Call::Edit(101, text, _, _)) if text == "⏱ 1:25 until Bench set 2/2"));
        sleep_ms(90_000).await;
        let calls = tg.calls();
        assert_eq!(calls[calls.len() - 2], Call::Remove(101));
        assert!(matches!(calls.last(), Some(Call::Send(text, _)) if text == "Bench · set 2"));
        assert_eq!(coach.shown().await, (Some("Bench · set 2".into()), 0));
    }

    #[tokio::test(start_paused = true)]
    async fn next_card_comes_at_rest_until_not_on_the_next_tick() {
        let tg = FakeTelegram::new();
        let coach = Coach::new(tg.clone(), 1, clock(), Some(next_card("Bench · set 2")), None);
        coach.apply(&view(vec![rest("until Bench set 2/2", "2026-10-02T18:00:07Z")]), None, None).await.unwrap();
        sleep_ms(8000).await;
        let timed = tg.timed();
        let (at, last) = timed.last().unwrap();
        assert!(matches!(last, Call::Send(text, _) if text == "Bench · set 2"));
        assert_eq!(*at, 7050, "last wait ends just past rest_until");
        assert_eq!(timed.iter().filter(|(_, c)| matches!(c, Call::Edit(..))).count(), 1, "one countdown edit at 5 s");
    }

    #[tokio::test(start_paused = true)]
    async fn shortened_rest_edits_the_same_countdown() {
        let tg = FakeTelegram::new();
        let coach = Coach::new(tg.clone(), 1, clock(), None, None);
        coach.apply(&view(vec![rest("until Bench set 2/2", "2026-10-02T18:01:30Z")]), None, None).await.unwrap();
        let shorter = CoachView { refresh: true, ..view(vec![rest("until Bench set 2/2", "2026-10-02T18:01:00Z")]) };
        coach.apply(&shorter, None, None).await.unwrap();
        assert!(matches!(tg.calls().last(), Some(Call::Edit(100, text, _, _)) if text == "⏱ 1:00 until Bench set 2/2"));
        sleep_ms(5001).await;
        let edits: Vec<String> = tg.calls().into_iter().filter_map(|c| if let Call::Edit(_, t, _, _) = c { Some(t) } else { None }).collect();
        assert_eq!(edits.last().unwrap(), "⏱ 0:55 until Bench set 2/2");
        assert_eq!(edits.len(), 2, "the old countdown stopped; only the new one ticks");
        assert_eq!(tg.calls().iter().filter(|c| matches!(c, Call::Send(..))).count(), 1, "no new message, no extra notification");
        coach.apply(&CoachView { keep: true, ..view(vec![]) }, None, None).await.unwrap();
        assert_eq!(coach.shown().await.0.unwrap(), "until Bench set 2/2");
    }

    #[tokio::test(start_paused = true)]
    async fn swap_menu_restore_and_choice() {
        let tg = FakeTelegram::new();
        let coach = Coach::new(tg.clone(), 1, clock(), None, None);
        coach.apply(&view(vec![card("Cable fly")]), None, None).await.unwrap();
        let menu = vec![vec![Button { text: "⏳ I'll wait".into(), data: "gc:wait:abcd1234".into() }]];
        let option = |t: &str| Card { kind: CardKind::Option, ..card(t) };
        let swap = CoachView { keep: true, live_buttons: Some(menu.clone()), ..view(vec![option("Option A"), option("Option B")]) };
        let tap = |id| Tapped { message_id: id, text: None };
        coach.apply(&swap, Some(&tap(100)), None).await.unwrap();
        assert_eq!(tg.calls()[1], Call::Edit(100, "Cable fly".into(), menu.clone(), false));
        assert_eq!(coach.shown().await.1, 2);
        coach.apply(&CoachView { restore: true, ..view(vec![]) }, Some(&tap(100)), None).await.unwrap();
        let calls = tg.calls();
        assert_eq!(&calls[calls.len() - 3..calls.len() - 1], &[Call::Remove(101), Call::Remove(102)]);
        assert!(matches!(calls.last(), Some(Call::Edit(100, _, buttons, _)) if buttons.len() == 1 && buttons[0][0].data.starts_with("gc:log")));
        coach.apply(&swap, Some(&tap(100)), None).await.unwrap();
        coach.apply(&CoachView { cleanup: Some(Cleanup::Delete), ..view(vec![card("Option A · set 1")]) }, Some(&tap(104)), None).await.unwrap();
        let removed: Vec<i64> = tg.calls().into_iter().filter_map(|c| if let Call::Remove(id) = c { Some(id) } else { None }).collect();
        assert_eq!(&removed[removed.len() - 3..], &[100, 103, 104]);
        assert_eq!(coach.shown().await, (Some("Option A · set 1".into()), 0));
    }

    #[tokio::test(start_paused = true)]
    async fn standalone_preview_leaves_the_live_card() {
        let tg = FakeTelegram::new();
        let coach = Coach::new(tg.clone(), 1, clock(), None, None);
        coach.apply(&view(vec![card("Bench · set 1")]), None, None).await.unwrap();
        let preview = Card { photo: Some("/x/pull.png".into()), kind: CardKind::Preview, ..card("Pull · Mon 10:30") };
        coach.apply(&CoachView { standalone: true, ..view(vec![preview]) }, None, None).await.unwrap();
        // The overview image goes out without a caption.
        assert!(matches!(&tg.calls()[..], [_, Call::Send(text, _)] if text.is_empty()));
        assert_eq!(coach.shown().await.0.unwrap(), "Bench · set 1");
    }

    #[tokio::test(start_paused = true)]
    async fn gone_countdown_stops_without_a_second_card() {
        let tg = Arc::new(FakeTelegram { gone: true, ..Arc::into_inner(FakeTelegram::new()).unwrap() });
        let fired = Arc::new(StdMutex::new(0));
        let counter = fired.clone();
        let rest_over: RestOver = Arc::new(move || {
            *counter.lock().unwrap() += 1;
            Box::pin(async { Some(view(vec![card("Leg extension · set 1")])) })
        });
        let coach = Coach::new(tg.clone(), 1, clock(), Some(rest_over), None);
        coach.apply(&view(vec![rest("until Leg press set 2/2", "2026-10-02T18:02:16Z")]), None, None).await.unwrap();
        sleep_ms(180_000).await;
        assert_eq!(*fired.lock().unwrap(), 0);
        assert_eq!(tg.calls().iter().filter(|c| matches!(c, Call::Send(..))).count(), 1);
        assert_eq!(coach.shown().await.0, None);
    }

    #[tokio::test(start_paused = true)]
    async fn photo_cards_are_edited_as_captions_and_old_taps_get_the_ack() {
        let tg = FakeTelegram::new();
        let coach = Coach::new(tg.clone(), 1, clock(), None, None);
        let photo = Card { photo: Some("/x/bench.png".into()), ..card("Bench · set 1") };
        coach.apply(&view(vec![photo]), None, None).await.unwrap();
        let resting = CoachView { ack: Some("✅ 10 × 40 kg".into()), ..view(vec![rest("until Bench set 2/2", "2099-01-01T00:00:00Z")]) };
        coach.apply(&resting, None, None).await.unwrap();
        assert_eq!(tg.calls()[1], Call::Edit(100, "Bench · set 1\n✅ 10 × 40 kg".into(), vec![], true));
        // A tap on a card from before a restart: only its visible text is known.
        let old = Tapped { message_id: 42, text: Some("Squat · set 3".into()) };
        coach.apply(&CoachView { ack: Some("✅ 5 × 100 kg".into()), ..view(vec![card("Squat · set 4")]) }, Some(&old), None).await.unwrap();
        assert!(tg.calls().contains(&Call::Edit(42, "Squat · set 3\n✅ 5 × 100 kg".into(), vec![], false)));
        assert!(tg.calls().contains(&Call::Remove(101)), "the countdown is deleted, not retired");
    }
}
