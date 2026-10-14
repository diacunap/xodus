//! Small desktop UI to browse the games tied to the logged-in account.
//!
//! The page (ui.html) talks to Rust through wry's IPC: it posts
//! `{id, cmd, arg}` and gets `window.xodus.reply({id, ok, data | error})` back.

mod catalog;
mod downloads;
mod launcher;
mod library;
mod saves;

use std::path::PathBuf;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};
use tao::dpi::{LogicalSize, Size};
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::window::WindowBuilder;
use wry::WebViewBuilder;
use xodus::tokens::TokenManager;

#[derive(Deserialize)]
struct Request {
    id: u64,
    cmd: String,
    #[serde(default)]
    arg: String,
}

struct Ctx {
    client: reqwest::Client,
    tokens: TokenManager,
    downloads: Arc<downloads::Downloads>,
    launcher: Arc<launcher::Launcher>,
    saves: Arc<saves::Saves>,
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_env("XODUS_LOG"))
        .init();

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");

    xodus::secrets::init_secrets().expect("Unable to initialize credentials");
    // xodus-cli and xodus-service ship next to xodus-ui (Contents/MacOS in the bundle).
    let exe = std::env::current_exe().expect("current exe");
    let app_dir = exe.parent().expect("exe dir").to_path_buf();
    let ctx = Arc::new(Ctx {
        client: reqwest::ClientBuilder::new()
            .user_agent(format!("xodus-ui/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap(),
        tokens: TokenManager::with_default_storage(),
        downloads: downloads::Downloads::load(app_dir.join("xodus-cli")),
        launcher: launcher::Launcher::load(app_dir),
        saves: Arc::new(saves::Saves::new(
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join("Games/xodus"),
        )),
    });
    rt.block_on(xodus::tokens::device::ensure_device_credentials(&ctx.client, &ctx.tokens));

    let event_loop = EventLoopBuilder::<String>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let window = WindowBuilder::new()
        .with_title("Xodus")
        .with_inner_size(Size::Logical(LogicalSize::new(1180.0, 780.0)))
        .with_min_inner_size(Size::Logical(LogicalSize::new(720.0, 500.0)))
        .build(&event_loop)
        .expect("window");

    let handle = rt.handle().clone();
    let webview = WebViewBuilder::new()
        .with_html(include_str!("ui.html"))
        .with_ipc_handler(move |request| {
            let Ok(req) = serde_json::from_str::<Request>(request.body()) else {
                tracing::warn!("ignoring malformed IPC payload");
                return;
            };
            let ctx = ctx.clone();
            let proxy = proxy.clone();
            handle.spawn(async move {
                let reply = match handle_cmd(&ctx, &req.cmd, &req.arg).await {
                    Ok(data) => json!({ "id": req.id, "ok": true, "data": data }),
                    Err(err) => {
                        tracing::error!(cmd = req.cmd, "{err}");
                        json!({ "id": req.id, "ok": false, "error": err.to_string() })
                    }
                };
                let sent = proxy.send_event(reply.to_string());
                tracing::debug!(cmd = req.cmd, ok = sent.is_ok(), "reply queued");
            });
        })
        .build(&window)
        .expect("webview");

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(reply) => {
                let res = webview.evaluate_script(&format!("window.xodus.reply({reply})"));
                tracing::debug!(bytes = reply.len(), ?res, "delivered reply");
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => *control_flow = ControlFlow::Exit,
            _ => {}
        }
    });
}

async fn handle_cmd(ctx: &Ctx, cmd: &str, arg: &str) -> Result<Value, catalog::Error> {
    match cmd {
        "status" => {
            let user = ctx.tokens.get_user().ok().map(|u| u.username);
            let cli = std::env::current_exe()?.with_file_name("xodus-cli");
            Ok(json!({ "user": user, "cli": cli }))
        }
        "search" => Ok(serde_json::to_value(catalog::search(&ctx.client, arg).await?)?),
        "library" => {
            let owned = library::owned(&ctx.client, &ctx.tokens).await?;
            let ids: Vec<String> = owned.iter().map(|(id, _)| id.clone()).collect();
            let games = catalog::products(&ctx.client, &ids).await?;
            let acquisition: std::collections::HashMap<_, _> = owned.into_iter().collect();
            let games = games
                .into_iter()
                .map(|g| {
                    let mut v = serde_json::to_value(&g).unwrap();
                    v["acquisition"] = json!(acquisition.get(&g.product_id));
                    v
                })
                .collect::<Vec<_>>();
            Ok(json!(games))
        }
        "gamepass" => {
            let ids = library::gamepass(&ctx.client).await?;
            Ok(serde_json::to_value(catalog::products(&ctx.client, &ids).await?)?)
        }
        "downloads" => ctx.downloads.list(&ctx.client).await,
        "download_start" => {
            ctx.downloads.start(serde_json::from_str(arg)?)?;
            Ok(Value::Null)
        }
        "download_cancel" => ctx.downloads.cancel(arg).map(|_| Value::Null),
        "download_remove" => ctx.downloads.remove(arg).map(|_| Value::Null),
        "reveal" => ctx.downloads.reveal(arg).await.map(|_| Value::Null),
        "launch" => ctx.launcher.launch(&ctx.client, arg).await.map(|_| Value::Null),
        "launch_status" => Ok(ctx.launcher.status()),
        "launch_stop" => ctx.launcher.stop().await.map(|_| Value::Null),
        "open_log" => ctx.launcher.open_log().await.map(|_| Value::Null),
        "get_settings" => Ok(ctx.launcher.settings()),
        "set_settings" => ctx.launcher.set_settings(arg),
        "saves_list" => {
            let (saves, path, busy) = (ctx.saves.clone(), arg.to_owned(), ctx.launcher.busy_reason());
            tokio::task::spawn_blocking(move || saves.list(&path, busy)).await?
        }
        "saves_snapshot" => {
            let (saves, req) = (ctx.saves.clone(), serde_json::from_str::<saves::SnapshotRequest>(arg)?);
            let v = tokio::task::spawn_blocking(move || saves.snapshot(&req)).await??;
            Ok(serde_json::to_value(v)?)
        }
        "saves_restore" => {
            let req = serde_json::from_str::<saves::VersionRequest>(arg)?;
            // Held across the copy: Play is refused until the restore is done.
            let _guard = ctx.launcher.saves_guard()?;
            let saves = ctx.saves.clone();
            tokio::task::spawn_blocking(move || saves.restore(&req, None)).await?
        }
        "saves_reveal" => {
            let path = ctx.saves.version_path(&serde_json::from_str(arg)?)?;
            let status = tokio::process::Command::new("open").arg(&path).status().await?;
            if !status.success() {
                return Err(format!("open exited with {status}").into());
            }
            Ok(Value::Null)
        }
        other => Err(format!("unknown command {other}").into()),
    }
}
