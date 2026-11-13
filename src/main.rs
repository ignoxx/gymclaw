use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use gymclaw::agent::openrouter::OpenRouter;
use gymclaw::agent::turnlog::TurnLog;
use gymclaw::agent::{Agent, ChatModel, ModelReply, TurnInput};
use gymclaw::config::Config;
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
    /// For diagnosing a prod turn: snapshot, rewind the rows the turn changed, replay.
    Replay {
        /// The database copy to answer from; its tools write to it, so never the bot's own.
        #[arg(long, env = "GYMCLAW_DB")]
        db: PathBuf,
        #[arg(long, env = "OPENROUTER_API_KEY", hide_env_values = true)]
        key: String,
        /// The owner's local time to answer at (ISO 8601).
        #[arg(long)]
        at: String,
        #[arg(long, env = "GYMCLAW_MODEL", default_value = "anthropic/claude-haiku-5.5")]
        model: String,
        #[arg(long, env = "GYMCLAW_PROVIDER")]
        provider: Option<String>,
    },
    /// Serve the read-only dashboard.
    Dashboard {
        #[arg(long, default_value = "127.0.0.1:8766")]
        addr: SocketAddr,
        #[arg(long, default_value = "gymclaw.localhost")]
        allowed_host: String,
    },
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
    let tx = conn.transaction()?;
    let profile_tz = gymclaw::domain::profile::get_profile(&tx)?.tz();
    let now = match now {
        Some(text) => Ts::parse(&text, Some(&profile_tz))?,
        None => Ts::now(),
    };
    let mut cx = tools::Ctx {
        db: &tx,
        now,
        request_id: request_id.unwrap_or_else(|| format!("cli:{}", db::new_id())),
        data_dir: &data_dir,
        effects: vec![],
    };
    let result = tools::call(&mut cx, tool, args)?;
    tx.commit()?;
    Ok(result)
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
        model: Arc::new(Tap(OpenRouter::new(key, provider)?)),
        vision_model: model.clone(),
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
        Command::Replay { db, key, at, model, provider } => replay(db, &key, &at, model, provider).await.map(Some),
        Command::Dashboard { addr, allowed_host } => {
            gymclaw::web::dashboard(Config::paths().1, addr, allowed_host).await.map(|_| None)
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
