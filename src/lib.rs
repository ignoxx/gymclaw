//! GymClaw: a private Telegram gym coach. One binary owns the bot, the agent, the scheduler and the
//! training domain over one SQLite file.
pub mod agent;
pub mod config;
pub mod db;
pub mod domain;
pub mod error;
pub mod providers;
pub mod runtime;
pub mod telegram;
pub mod time;
pub mod tools;
pub mod web;
