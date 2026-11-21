use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use gymclaw::agent::openrouter::OpenRouter;
use gymclaw::agent::turnlog::TurnLog;
use gymclaw::agent::{Agent, ChatModel, ModelReply, TurnInput};
use gymclaw::config::{self, Config};
use gymclaw::error::{Error, Result};
use gymclaw::time::Ts;
use gymclaw::{db, tools};
use serde_json::{Value, json};

#[derive(Parser)]
#[command(name = "gymclaw", about = "Private Telegram gym coach")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the bot (Telegram, agent, scheduler, outbox).
    Serve,
    /// Create or upgrade the database schema.
    Migrate,
    /// Run one tool, exactly as the agent would: `gymclaw call session_update '{"session_id":"…","fields":{"status":"CANCELLED"}}'`.
    Call {
        tool: String,
        /// JSON arguments (default `{}`); `-` reads them from stdin, so secrets stay out of argv.
        args: Option<String>,
        /// Pretend it is this time (ISO 8601 with offset).
        #[arg(long)]
        now: Option<String>,
        /// Idempotency key; a retry with the same key returns the first result.
        #[arg(long)]
        request_id: Option<String>,
    },
    /// List tools with their argument schemas.
    Tools,
    /// Consistent copy of the database (default `data/backups/manual-<time>.db`).
    Backup { dest: Option<PathBuf> },
    /// Authorize Google Calendar (first time, or after CALENDAR_AUTH_REQUIRED). Prints a URL; open it in
    /// any browser, approve, then paste the URL the browser ends up on (an error page on 127.0.0.1).
    CalendarAuth {
        /// The dedicated calendar's ID (never `primary`).
        #[arg(long, env = "GOOGLE_CALENDAR_ID")]
        calendar_id: String,
        /// Desktop-app OAuth client JSON downloaded from Google Cloud.
        #[arg(long, default_value = "data/google-client.json")]
        client_file: PathBuf,
    },
    /// Re-run the agent's turn on a database copy: the chat's newest owner message,
    /// answered at `--at`, with `--model`. Tools write to that copy; every model step prints to stderr.
    /// For diagnosing a prod turn: snapshot, rewind the rows the turn changed (`rewind`), replay.
    Replay {
        /// The database copy to answer from; its tools write to it, so never the bot's own.
        #[arg(long, env = "GYMCLAW_DB")]
        db: PathBuf,
        #[arg(long, env = "OPENROUTER_API_KEY", hide_env_values = true)]
        key: String,
        /// The owner's local time to answer at (ISO 8601).
        #[arg(long)]
        at: String,
        /// Default: GYMCLAW_MODEL, else the bot's default.
        #[arg(long)]
        model: Option<String>,
        /// Default: GYMCLAW_PROVIDER.
        #[arg(long)]
        provider: Option<String>,
    },
    /// Undo one agent turn's tool calls on a database copy, from the inverse changesets in the turn
    /// log (`run_end.run`). Rewind later turns first; a row changed since aborts without writing.
    Rewind {
        run: String,
        /// The database copy to rewind; never the bot's own.
        #[arg(long, env = "GYMCLAW_DB")]
        db: PathBuf,
        /// A local copy of the turn log (`scripts/pull-turns`).
        #[arg(long, env = "GYMCLAW_TURN_LOG_DIR")]
        turns: PathBuf,
    },
    /// Index every past agent turn with outcome labels, as JSONL: exact captures by reference, older
    /// turns rebuilt from the turn log, chat and mutations (`source: backfill` + fidelity). Read-only;
    /// run it on copies (docs/turns.md).
    Backfill {
        #[arg(long, env = "GYMCLAW_DB")]
        db: PathBuf,
        #[arg(long, env = "GYMCLAW_TURN_LOG_DIR")]
        turns: PathBuf,
        #[arg(long)]
        out: PathBuf,
        /// A clone of this repo: each backfilled turn gets the commit that served it (merge time).
        #[arg(long)]
        git: Option<PathBuf>,
    },
    /// Every logged set as CSV in Strong's format (Hevy and others import it); to stdout, or `path`.
    Export { path: Option<PathBuf> },
    /// Exercise pictures. `missing`: exercises with no picture of their own (plans first, then by
    /// logged sets), the work list for scripts/new-illustration.
    Illustrations {
        #[command(subcommand)]
        command: IllustrationsCommand,
    },
    /// Serve the dashboard. Its only writes: the agent's notes, its extensions' on/off switch and plan targets.
    Dashboard {
        #[arg(long, default_value = "127.0.0.1:8766")]
        addr: SocketAddr,
        #[arg(long, default_value = "gymclaw.localhost")]
        allowed_host: String,
    },
    /// Verify environment, database, Telegram token, OpenRouter key and optional integrations.
    Doctor,
    /// Test the configured gym crowd source once and print the count without writing to the DB.
    CrowdTest,
    /// Clear the bound gym crowd source so a different gym/source config can be used without a fresh DB.
    CrowdReset,
}

#[derive(Subcommand)]
enum IllustrationsCommand {
    Missing,
}

fn calendar_auth(calendar_id: &str, client_file: &std::path::Path) -> Result<Value> {
    use gymclaw::providers::google::Consent;
    let consent = Consent::new(&std::fs::read(client_file).map_err(|_| {
        Error::domain("CALENDAR_AUTH_FAILED", format!("Client file not found: {}", client_file.display()))
    })?)?;
    eprintln!(
        "Open this URL, approve access, then paste the address your browser ends up on (it won't load):\n\n{}\n",
        consent.url()
    );
    eprint!("Pasted URL: ");
    let mut pasted = String::new();
    std::io::stdin().read_line(&mut pasted)?;
    let conn = db::open(&Config::paths().1)?;
    consent.finish(&conn, calendar_id, &pasted, &gymclaw::providers::http::Net::new()?, Ts::now())
}

fn crowd_test() -> Result<Value> {
    let net = gymclaw::providers::http::Net::new()?;
    let provider = gymclaw::providers::crowd::from_environment(&net, &gymclaw::providers::crowd::run_command)?;
    let reading = provider.get_reading()?;
    Ok(json!({
        "ok": true,
        "provider_id": provider.provider_id(),
        "count": reading.raw_value,
        "freshness_seconds": reading.freshness_seconds,
    }))
}

async fn doctor() -> Value {
    let (_, db_path) = Config::paths();
    let db_check = if !db_path.exists() {
        json!({"ok": false, "path": db_path, "error": "database file does not exist (run `gymclaw migrate`)"})
    } else {
        match db::open_readonly(&db_path) {
            Ok(conn) => {
                let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap_or(0);
                let setup_done = gymclaw::domain::onboarding::finished(&conn).unwrap_or(false);
                let runtime = gymclaw::runtime::outbox::runtime_authority(&conn).ok();
                let google = gymclaw::db::models::CalendarSyncState::get(&conn, 1)
                    .ok()
                    .flatten()
                    .is_some_and(|s| s.source == "google");
                let ics =
                    gymclaw::domain::personal_calendar::status(&conn).ok().is_some_and(|s| s["connected"] == true);
                json!({
                    "ok": version > 0, "path": db_path, "schema_version": version, "setup_done": setup_done,
                    "runtime_enabled": runtime.as_ref().is_some_and(|r| r.enabled),
                    "google_calendar": google, "personal_ics": ics,
                })
            }
            Err(e) => json!({"ok": false, "path": db_path, "error": e.to_string()}),
        }
    };
    let owner = config::owner_id();
    let tg_check = match config::telegram_token() {
        None => json!({"ok": false, "error": "TELEGRAM_BOT_TOKEN not set"}),
        Some(token) => match reqwest::Client::new()
            .get(format!("https://api.telegram.org/bot{token}/getMe"))
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                let body: Value = resp.json().await.unwrap_or(Value::Null);
                json!({"ok": body["ok"] == true, "username": body.pointer("/result/username")})
            }
            Ok(resp) => json!({"ok": false, "status": resp.status().as_u16()}),
            Err(e) => json!({"ok": false, "error": e.without_url().to_string()}),
        },
    };
    let openrouter = json!({"ok": config::openrouter_key().is_some()});
    let crowd_path = config::crowd_config();
    let crowd = json!({"configured": crowd_path.exists(), "path": crowd_path});
    let ok = db_check["ok"] == true && owner.is_ok() && tg_check["ok"] == true && openrouter["ok"] == true;
    json!({
        "ok": ok,
        "owner_id_set": owner.is_ok(),
        "db": db_check,
        "telegram": tg_check,
        "openrouter": openrouter,
        "crowd": crowd,
    })
}

fn print(value: &Value) {
    println!("{}", serde_json::to_string(value).expect("JSON output"));
}

fn call(tool: &str, args: Option<String>, now: Option<String>, request_id: Option<String>) -> Result<Value> {
    let (data_dir, db_path) = Config::paths();
    let mut conn = db::open(&db_path)?;
    let args = match args.as_deref() {
        Some("-") => Some(std::io::read_to_string(std::io::stdin())?),
        _ => args,
    };
    let args: Value = match args {
        Some(text) => serde_json::from_str(&text).map_err(|e| Error::domain("INVALID_JSON", e.to_string()))?,
        None => json!({}),
    };
    db::in_tx(&mut conn, |tx| {
        let now = match now {
            Some(text) => Ts::parse(&text, Some(&gymclaw::domain::profile::get_profile(tx)?.tz()))?,
            None => Ts::now(),
        };
        let request_id = request_id.unwrap_or_else(|| format!("cli:{}", db::new_id()));
        let recipient = config::owner_id().ok().map(|id| id.to_string());
        tools::call(
            &mut tools::Ctx { db: tx, now, request_id, data_dir: &data_dir, recipient, effects: vec![] },
            tool,
            args,
        )
    })
}

/// Prints each model step (reasoning, text, tool calls) as it happens.
struct Tap(OpenRouter);

#[async_trait::async_trait]
impl ChatModel for Tap {
    async fn complete(&self, model: &str, messages: &[Value], tools: &[Value], force_text: bool) -> Result<ModelReply> {
        let reply =
            self.0.complete(model, messages, tools, force_text).await.inspect_err(|e| eprintln!("model error: {e}"))?;
        let m = &reply.message;
        let u = &reply.usage;
        eprintln!(
            "--- step ({} ms; tokens in {:?}, cached {:?}, out {:?}, reasoning {:?})",
            reply.ms, u.input, u.cache_read, u.output, u.reasoning
        );
        for key in ["reasoning", "content"] {
            if let Some(text) = m[key].as_str().filter(|t| !t.trim().is_empty()) {
                eprintln!("{key}: {text}");
            }
        }
        for call in m["tool_calls"].as_array().into_iter().flatten() {
            eprintln!("call {} {}", call["function"]["name"].as_str().unwrap_or("?"), call["function"]["arguments"]);
        }
        Ok(reply)
    }
}

async fn replay(db_path: PathBuf, key: &str, at: &str, model: String, provider: Option<String>) -> Result<Value> {
    let tz = gymclaw::domain::profile::get_profile(&db::open(&db_path)?)?.tz();
    let now = Ts::parse(at, Some(&tz))?;
    let data_dir = db_path.parent().map(|p| p.join("replay")).unwrap_or_else(|| "replay".into());
    let agent = Agent {
        db_path,
        data_dir,
        recipient: config::owner_id().ok().map(|id| id.to_string()),
        model: Arc::new(Tap(OpenRouter::new(key, provider)?)),
        vision_model: model.clone(),
        audio_model: model.clone(),
        chat_model: model,
        coach: None,
        log: TurnLog::default(),
        clock: Arc::new(move || now),
    };
    let outcome =
        agent.turn(TurnInput { key: format!("replay-{}", db::new_id()), images: vec![], background: false }).await;
    Ok(
        json!({"failed": outcome.failed, "reply": outcome.reply.message.map(|m| m.text), "react": outcome.reply.react, "saved": outcome.actions}),
    )
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let cli = Cli::parse();
    let result: Result<Option<Value>> = match cli.command {
        Command::Serve => match Config::from_env() {
            Ok(config) => gymclaw::runtime::serve(config).await.map(|_| None),
            Err(message) => Err(Error::domain("CONFIG", message)),
        },
        Command::Migrate => {
            let (_, path) = Config::paths();
            db::open(&path).and_then(|mut conn| db::migrate(&mut conn)).map(|_| Some(json!({"ok": true})))
        }
        Command::Call { tool, args, now, request_id } => {
            tokio::task::spawn_blocking(move || call(&tool, args, now, request_id))
                .await
                .expect("tool call thread")
                .map(|r| {
                    let mut out = json!({"ok": true});
                    match r {
                        Value::Object(map) if map.contains_key("data") => out.as_object_mut().unwrap().extend(map),
                        other => out["data"] = other,
                    }
                    Some(out)
                })
        }
        Command::Tools => Ok(Some(Value::Array(
            tools::all()
                .iter()
                .map(|t| json!({"name": t.name, "description": t.description, "parameters": (t.schema)()}))
                .collect(),
        ))),
        Command::Backup { dest } => {
            let (data_dir, path) = Config::paths();
            let dest =
                dest.unwrap_or_else(|| data_dir.join("backups").join(format!("manual-{}.db", Ts::now().0.as_second())));
            db::open(&path).and_then(|conn| db::snapshot(&conn, &dest)).map(|_| Some(json!({"ok": true, "path": dest})))
        }
        Command::CalendarAuth { calendar_id, client_file } => {
            tokio::task::spawn_blocking(move || calendar_auth(&calendar_id, &client_file))
                .await
                .expect("calendar auth thread")
                .map(Some)
        }
        Command::Replay { db, key, at, model, provider } => {
            let (model, provider) = (model.unwrap_or_else(config::model), provider.or_else(config::provider));
            replay(db, &key, &at, model, provider).await.map(Some)
        }
        Command::Rewind { run, db, turns } => gymclaw::agent::turnlog::read(&turns).and_then(|events| {
            let undone = gymclaw::agent::turnlog::rewind(&mut db::open(&db)?, &events, &run)?;
            Ok(Some(json!({"ok": true, "run": run, "tool_calls_undone": undone})))
        }),
        Command::Backfill { db, turns, out, git } => (|| {
            use gymclaw::agent::{backfill, turnlog};
            let releases = git.as_deref().map(backfill::releases).transpose()?.unwrap_or_default();
            let records = backfill::build(&db::open_readonly(&db)?, &turnlog::read(&turns)?, &releases)?;
            backfill::write(&out, &records)?;
            let count = |source: &str| records.iter().filter(|r| r["source"] == source).count();
            Ok(Some(json!({"ok": true, "out": out, "captured": count("capture"), "backfilled": count("backfill")})))
        })(),
        Command::Export { path } => {
            let csv = db::open(&Config::paths().1).and_then(|conn| gymclaw::domain::import::export_csv(&conn));
            match (csv, path) {
                (Ok(csv), Some(path)) => {
                    std::fs::write(&path, csv).map_err(Error::from).map(|_| Some(json!({"ok": true, "path": path})))
                }
                (Ok(csv), None) => {
                    print!("{csv}");
                    Ok(None)
                }
                (Err(e), _) => Err(e),
            }
        }
        Command::Illustrations { command: IllustrationsCommand::Missing } => db::open(&Config::paths().1)
            .and_then(|conn| gymclaw::domain::illustrations::missing(&conn))
            .map(|missing| Some(json!(missing))),
        Command::Dashboard { addr, allowed_host } => {
            let auth = match config::telegram_token() {
                Some(bot_token) => match config::owner_id() {
                    Ok(owner_id) => Some(gymclaw::web::WebAuth { bot_token, owner_id }),
                    Err(msg) => {
                        return {
                            print(&Error::domain("CONFIG", msg).to_json());
                            ExitCode::FAILURE
                        };
                    }
                },
                None => None,
            };
            let (data_dir, db_path) = Config::paths();
            gymclaw::web::dashboard(db_path, data_dir, addr, allowed_host, auth).await.map(|_| None)
        }
        Command::Doctor => {
            let report = doctor().await;
            let ok = report["ok"] == true;
            print(&report);
            return if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE };
        }
        Command::CrowdTest => tokio::task::spawn_blocking(crowd_test).await.expect("crowd test thread").map(Some),
        Command::CrowdReset => {
            let (_, path) = Config::paths();
            db::open(&path)
                .and_then(|mut conn| db::in_tx(&mut conn, gymclaw::domain::crowd::reset_source))
                .map(|cleared| Some(json!({"ok": true, "cleared": cleared})))
        }
    };
    match result {
        Ok(Some(value)) => {
            print(&value);
            ExitCode::SUCCESS
        }
        Ok(None) => ExitCode::SUCCESS,
        Err(error) => {
            print(&error.to_json());
            ExitCode::FAILURE
        }
    }
}
