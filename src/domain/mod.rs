//! Training domain. Synchronous functions over a caller-owned transaction (`&Connection`).
pub mod core;
pub mod view;

pub mod illustrations;
pub mod profile;
pub mod progression;
pub mod set_parser;
pub mod templates;

// WP-train
pub mod adaptation;
pub mod audit;
pub mod backfill;
pub mod body;
pub mod coach;
pub mod onboarding;
pub mod preview;
pub mod workout;

// Lead (planning)
pub mod availability;
pub mod calendar;
pub mod calendar_busy;
pub mod calendar_writes;
pub mod compression;
pub mod crowd;
pub mod events;
pub mod habits;
pub mod notifications;
pub mod personal_calendar;
pub mod planning;
pub mod replanning;
pub mod scheduling;
pub mod ticks;
pub mod weekly;
