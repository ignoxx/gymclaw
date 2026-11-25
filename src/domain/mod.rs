//! Training domain. Synchronous functions over a caller-owned transaction (`&Connection`).
pub mod core;
pub mod view;

pub mod illustrations;
pub mod profile;
pub mod progression;
pub mod set_parser;
pub mod templates;

// Training
pub mod adaptation;
pub mod audit;
pub mod body;
pub mod coach;
pub mod equipment;
pub mod highlights;
pub mod import;
pub mod onboarding;
pub mod past;
pub mod preview;
pub mod trends;
pub mod workout;

// Planning
pub mod activities;
pub mod availability;
pub mod calendar;
pub mod calendar_busy;
pub mod compression;
pub mod crowd;
pub mod events;
pub mod extensions;
pub mod goals;
pub mod habits;
pub mod notifications;
pub mod personal_calendar;
pub mod planning;
pub mod replanning;
pub mod scheduling;
pub mod ticks;
pub mod weekly;
