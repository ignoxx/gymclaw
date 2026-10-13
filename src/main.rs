use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
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
    /// Run one tool, exactly as the agent would: `gymclaw call move_session '{"session_id":"…"}'`.
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
    let consent = Consent::new(&std::fs::read(client_file).map_err(|_| Error::domain("CALENDAR_AUTH_FAILED", format!("Client file not found: {}", client_file.display())))?)?;
    eprintln!("Open this URL, approve access, then paste the address your browser ends up on (it won't load):\n\n{}\n", consent.url());
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
    let def = tools::all().into_iter().find(|t| t.name == tool);
    if def.as_ref().is_some_and(|d| d.snapshot) {
        db::snapshot(&conn, &data_dir.join("backups").join(format!("cli-{}.db", Ts::now().0.as_second())))?;
    }
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
    let mut cx = tools::Ctx { db: &tx, now, request_id: request_id.unwrap_or_else(|| format!("cli:{}", db::new_id())), data_dir: &data_dir, effects: vec![] };
    let result = tools::call(&mut cx, tool, args)?;
    tx.commit()?;
    Ok(result)
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into())).init();
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
        Command::Call { tool, args, now, request_id } => tokio::task::spawn_blocking(move || call(&tool, args, now, request_id))
            .await
            .expect("tool call thread")
            .map(|r| {
                let mut out = json!({"ok": true});
                match r {
                    Value::Object(map) if map.contains_key("data") => out.as_object_mut().unwrap().extend(map),
                    other => out["data"] = other,
                }
                Some(out)
            }),
        Command::Tools => Ok(Some(Value::Array(tools::all().iter().map(|t| json!({"name": t.name, "description": t.description, "parameters": (t.schema)()})).collect()))),
        Command::Backup { dest } => {
            let (data_dir, path) = Config::paths();
            let dest = dest.unwrap_or_else(|| data_dir.join("backups").join(format!("manual-{}.db", Ts::now().0.as_second())));
            db::open(&path).and_then(|conn| db::snapshot(&conn, &dest)).map(|_| Some(json!({"ok": true, "path": dest})))
        }
        Command::CalendarAuth { calendar_id, client_file } => tokio::task::spawn_blocking(move || calendar_auth(&calendar_id, &client_file))
            .await
            .expect("calendar auth thread")
            .map(Some),
        Command::Dashboard { addr, allowed_host } => gymclaw::web::dashboard(Config::paths().1, addr, allowed_host).await.map(|_| None),
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
