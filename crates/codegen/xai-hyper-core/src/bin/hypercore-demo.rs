//! Phase 1 demo: multi-turn chat via Hypercore + NativeHost.
//!
//! ```sh
//! export XAI_API_KEY=...   # or rely on ~/.grok/auth.json
//! cargo run -p xai-hyper-core --bin hypercore-demo --features native
//! ```
//!
//! Each non-empty stdin line is one turn. Prefix with `turn_id|` to set an
//! idempotent id (default: auto uuid-ish). Empty line or `quit` exits.
//!
//! Env:
//! - `XAI_API_KEY` / `HYPERCORE_API_KEY`
//! - `HYPERCORE_MODEL` (default grok-4)
//! - `HYPERCORE_BASE_URL` (default https://api.x.ai/v1)
//! - `HYPERCORE_API_BACKEND` (`chat_completions` | `responses` | `codex_responses`)
//! - `HYPERCORE_SESSION` session id (default `demo`)

use std::io::{self, BufRead, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use xai_hyper_core::native::NativeHost;
use xai_hyper_core::{CoreConfig, CoreEvent, HyperCore, TurnRequest};

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("hypercore-demo: {e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let session = std::env::var("HYPERCORE_SESSION").unwrap_or_else(|_| "demo".into());
    let host = NativeHost::from_env();
    let model = host.model().to_string();

    println!("hypercore-demo  session={session}  model={model}");
    println!("storage: {}", host.hypercore_root().join(&session).display());
    println!("type a message (or turn_id|message). empty / quit to exit.\n");

    let mut core = HyperCore::restore_or_new(
        host.clone(),
        session,
        CoreConfig {
            model,
            max_messages: 128,
        },
    )
    .await?;

    if core.completed_turns() > 0 {
        println!(
            "(restored {} turns, {} messages)\n",
            core.completed_turns(),
            core.items().len()
        );
    }

    let stdin = io::stdin();
    let mut stdout = io::stdout();
    loop {
        print!("you> ");
        stdout.flush()?;
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim();
        if line.is_empty() || line.eq_ignore_ascii_case("quit") || line.eq_ignore_ascii_case("exit")
        {
            break;
        }

        let (turn_id, text) = match line.split_once('|') {
            Some((id, rest)) if !id.trim().is_empty() => {
                (id.trim().to_string(), rest.trim().to_string())
            }
            _ => (auto_turn_id(), line.to_string()),
        };

        print!("assistant> ");
        stdout.flush()?;

        match core
            .submit_turn(TurnRequest {
                turn_id: turn_id.clone(),
                text,
                json_schema: None,
                tools: None,
            })
            .await
        {
            Ok(out) => {
                for ev in &out.events {
                    if let CoreEvent::AssistantDelta { text, .. } = ev {
                        print!("{text}");
                        stdout.flush()?;
                    }
                }
                println!();
                if out.replayed {
                    println!("(replayed turn_id={turn_id}, streams={})", host.model_stream_opens());
                } else {
                    println!(
                        "(committed turn_id={turn_id}, streams={})",
                        host.model_stream_opens()
                    );
                }
                println!();
            }
            Err(e) => {
                println!();
                eprintln!("error: {e}");
            }
        }
    }

    println!("bye. completed_turns={}", core.completed_turns());
    Ok(())
}

fn auto_turn_id() -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("t-{ms}")
}
