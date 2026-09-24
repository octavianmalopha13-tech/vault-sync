use anyhow::{bail, Context, Result};
use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
    routing::get,
    Router,
};
use clap::{Parser, Subcommand};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

// ---------- CLI ----------

#[derive(Parser)]
#[command(name = "vault-sync")]
#[command(about = "Sync a pass-manager vault between devices")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the sync server
    Serve {
        #[arg(long, default_value = "127.0.0.1:8080")]
        bind: String,
    },
    /// Push a local file to the server
    Push {
        file: PathBuf,
        #[arg(long)]
        server: String,
    },
    /// Pull the server's blob into a local file
    Pull {
        file: PathBuf,
        #[arg(long)]
        server: String,
    },
    /// Merge a remote vault into a local one (client-side merge)
Merge {
    file: PathBuf,
    #[arg(long)]
    server: String,
   // #[arg(long)]
   // password: String,
},

}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Serve { bind } => cmd_serve(&bind).await,
        Commands::Push { file, server } => cmd_push(&file, &server).await,
        Commands::Pull { file, server } => cmd_pull(&file, &server).await,
        Commands::Merge { file, server} => cmd_merge(&file, &server).await,
    }
}

// ---------- server ----------

struct ServerState {
    inner: Mutex<ServerInner>,
}

#[derive(Default)]
struct ServerInner {
    blob: Vec<u8>,
    version: u64,
}

async fn cmd_serve(bind: &str) -> Result<()> {
    let state = Arc::new(ServerState {
        inner: Mutex::new(ServerInner::default()),
    });

    let app = Router::new()
        .route("/vault", get(get_vault).put(put_vault))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding {}", bind))?;
    println!("Listening on http://{}", bind);
    axum::serve(listener, app).await?;
    Ok(())
}

async fn get_vault(State(state): State<Arc<ServerState>>) -> impl IntoResponse {
    let inner = state.inner.lock().await;
    if inner.blob.is_empty() {
        return (StatusCode::NOT_FOUND, HeaderMap::new(), Vec::new());
    }
    let mut h = HeaderMap::new();
    h.insert(
        "x-vault-version",
        HeaderValue::from_str(&inner.version.to_string()).unwrap(),
    );
    (StatusCode::OK, h, inner.blob.clone())
}

async fn put_vault(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let client_version: u64 = headers
        .get("x-vault-version")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let mut inner = state.inner.lock().await;

    if client_version != inner.version {
        // Client is out of date. Report the actual server version.
        let mut h = HeaderMap::new();
        h.insert(
            "x-vault-version",
            HeaderValue::from_str(&inner.version.to_string()).unwrap(),
        );
        return (StatusCode::CONFLICT, h, Vec::new());
    }

    inner.blob = body.to_vec();
    inner.version += 1;

    let mut h = HeaderMap::new();
    h.insert(
        "x-vault-version",
        HeaderValue::from_str(&inner.version.to_string()).unwrap(),
    );
    (StatusCode::OK, h, Vec::new())
}

// ---------- client state sidecar ----------

fn state_path(file: &Path) -> PathBuf {
    let mut p = file.to_path_buf();
    let name = p
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "vault".into());
    p.set_file_name(format!("{}.sync-state", name));
    p
}

fn read_version(file: &Path) -> u64 {
    fs::read_to_string(state_path(file))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

fn write_version(file: &Path, version: u64) -> Result<()> {
    fs::write(state_path(file), format!("{}\n", version))?;
    Ok(())
}

// ---------- client commands ----------

async fn cmd_push(file: &Path, server: &str) -> Result<()> {
    let blob = fs::read(file).with_context(|| format!("reading {}", file.display()))?;
    let local_version = read_version(file);

    let client = reqwest::Client::new();
    let resp = client
        .put(format!("{}/vault", server.trim_end_matches('/')))
        .header("x-vault-version", local_version.to_string())
        .body(blob)
        .send()
        .await?;

    let status = resp.status();
    let server_version: u64 = resp
        .headers()
        .get("x-vault-version")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    match status {
        reqwest::StatusCode::OK => {
            write_version(file, server_version)?;
            println!("Pushed. Server is now at version {}.", server_version);
            Ok(())
        }
        reqwest::StatusCode::CONFLICT => {
            bail!(
                "Conflict: server is at version {}; your local state is {}.\n\
                 Pull first to update, then retry.",
                server_version,
                local_version
            );
        }
        s => bail!("unexpected status {} (server version {})", s, server_version),
    }
}

async fn cmd_pull(file: &Path, server: &str) -> Result<()> {
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{}/vault", server.trim_end_matches('/')))
        .send()
        .await?;

    let status = resp.status();
    let server_version: u64 = resp
        .headers()
        .get("x-vault-version")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    match status {
        reqwest::StatusCode::OK => {
            let bytes = resp.bytes().await?;
            fs::write(file, &bytes).with_context(|| format!("writing {}", file.display()))?;
            write_version(file, server_version)?;
            println!("Pulled version {} into {}", server_version, file.display());
            Ok(())
        }
        reqwest::StatusCode::NOT_FOUND => {
            bail!("server has no vault yet — push from another device first");
        }
        s => bail!("unexpected status {} (server version {})", s, server_version),
    }
}

use vault_core::{self as vault, Vault};

async fn cmd_merge(file: &Path, server: &str) -> Result<()> {
    let password = rpassword::prompt_password("Master password: ")?;
    // Load local vault
    let local = vault::load_vault(&file.to_string_lossy(), &password)
        .with_context(|| format!("loading local vault {}", file.display()))?;

    let local_version = read_version(file);

    // Fetch remote blob
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{}/vault", server.trim_end_matches('/')))
        .send()
        .await?;

    let server_version: u64 = resp
        .headers()
        .get("x-vault-version")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        println!("Server empty — pushing local vault.");
        // fall through to push
    }

    let remote: Vault = if resp.status() == reqwest::StatusCode::OK {
        let bytes = resp.bytes().await?;
        // Remote is an encrypted vault blob. We need to decrypt it to merge.
        // Easiest: write to a temp file and use vault::load_vault.
        let tmp = std::env::temp_dir().join("vault-sync-merge.tmp");
        fs::write(&tmp, &bytes)?;
        let v = vault::load_vault(&tmp.to_string_lossy(), &password)
            .with_context(|| "decrypting remote vault (wrong password?)")?;
        let _ = fs::remove_file(&tmp);
        v
    } else {
        Vault::default()
    };

    // --- THE MERGE ---
    // Start from local, then walk remote and add anything we don't have.
    // For entries present in both but different, keep local and warn.
    let mut merged = Vault {
        entries: local.entries.clone(),
    };
    let mut conflicts = Vec::new();

    for r in &remote.entries {
        match merged.find(&r.site) {
            None => merged.entries.push(r.clone()),
            Some(l) => {
                if l != r {
                    conflicts.push(r.site.clone());
                    // local wins
                }
            }
        }
    }

    if !conflicts.is_empty() {
        eprintln!("Conflicts on (local wins): {}", conflicts.join(", "));
    }

    // Write the merged vault locally with a fresh save (new salt, new nonce)
    vault::save_vault(&file.to_string_lossy(), &merged, &password)?;

    // Push merged to server, with the current server version
    let blob = fs::read(file)?;
    let push_resp = client
        .put(format!("{}/vault", server.trim_end_matches('/')))
        .header("x-vault-version", server_version.to_string())
        .body(blob)
        .send()
        .await?;

    let new_server_version: u64 = push_resp
        .headers()
        .get("x-vault-version")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    match push_resp.status() {
        reqwest::StatusCode::OK => {
            write_version(file, new_server_version)?;
            println!(
                "Merged local ({}) + remote ({}), pushed. Server now at version {}.",
                local_version, server_version, new_server_version
            );
            Ok(())
        }
        reqwest::StatusCode::CONFLICT => {
            bail!("race: server moved during merge, retry")
        }
        s => bail!("push failed: {}", s),
    }
}
