use anyhow::{bail, Context, Result};
use axum::{
    body::Bytes,
    extract::{Path as AxumPath, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
    routing::get,
    Router,
};
use clap::{Parser, Subcommand};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
        #[arg(long, default_value = "sync-server")]
        data_dir: PathBuf,
    },
    /// Push a local file to the server
    Push {
        file: PathBuf,
        #[arg(long)]
        server: String,
        #[arg(long)]
        vault_name: Option<String>,
    },
    /// Pull the server's blob into a local file
    Pull {
        file: PathBuf,
        #[arg(long)]
        server: String,
        #[arg(long)]
        vault_name: Option<String>,
    },
    /// Merge a remote vault into a local one (client-side merge)
    Merge {
        file: PathBuf,
        #[arg(long)]
        server: String,
        #[arg(long)]
        vault_name: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Serve { bind, data_dir } => cmd_serve(&bind, &data_dir).await,
        Commands::Push { file, server, vault_name } => {
            cmd_push(&file, &server, vault_name.as_deref()).await
        }
        Commands::Pull { file, server, vault_name } => {
            cmd_pull(&file, &server, vault_name.as_deref()).await
        }
        Commands::Merge { file, server, vault_name } => {
            cmd_merge(&file, &server, vault_name.as_deref()).await
        }
    }
}

// ---------- server ----------

struct ServerState {
    data_dir: PathBuf,
}

impl ServerState {
    fn vault_dir(&self, name: &str) -> PathBuf {
        self.data_dir.join("vaults").join(name)
    }

    fn read_version(&self, name: &str) -> u64 {
        fs::read_to_string(self.vault_dir(name).join("version"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }

    fn read_blob(&self, name: &str) -> Option<Vec<u8>> {
        fs::read(self.vault_dir(name).join("blob")).ok()
    }

    fn write_vault(&self, name: &str, blob: &[u8], version: u64) -> Result<()> {
        let dir = self.vault_dir(name);
        fs::create_dir_all(&dir)?;

        // Write blob atomically via a temp file, then bump version.
        let tmp = dir.join("blob.tmp");
        fs::write(&tmp, blob)?;
        fs::rename(&tmp, dir.join("blob"))?;

        fs::write(dir.join("version"), format!("{}\n", version))?;
        Ok(())
    }
}

async fn cmd_serve(bind: &str, data_dir: &Path) -> Result<()> {
    fs::create_dir_all(data_dir.join("vaults"))
        .with_context(|| format!("creating data dir {}", data_dir.display()))?;

    let state = Arc::new(ServerState {
        data_dir: data_dir.to_path_buf(),
    });

    let app = Router::new()
        .route("/vaults/{name}", get(get_vault).put(put_vault))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding {}", bind))?;
    println!("Data directory: {}", data_dir.display());
    println!("Listening on http://{}", bind);
    axum::serve(listener, app).await?;
    Ok(())
}

async fn get_vault(
    State(state): State<Arc<ServerState>>,
    AxumPath(name): AxumPath<String>,
) -> impl IntoResponse {
    match state.read_blob(&name) {
        None => (StatusCode::NOT_FOUND, HeaderMap::new(), Vec::new()),
        Some(blob) => {
            let version = state.read_version(&name);
            let mut h = HeaderMap::new();
            h.insert(
                "x-vault-version",
                HeaderValue::from_str(&version.to_string()).unwrap(),
            );
            (StatusCode::OK, h, blob)
        }
    }
}

async fn put_vault(
    State(state): State<Arc<ServerState>>,
    AxumPath(name): AxumPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let client_version: u64 = headers
        .get("x-vault-version")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let current = state.read_version(&name);

    if client_version != current {
        let mut h = HeaderMap::new();
        h.insert(
            "x-vault-version",
            HeaderValue::from_str(&current.to_string()).unwrap(),
        );
        return (StatusCode::CONFLICT, h, Vec::new());
    }

    let next = current + 1;
    if let Err(e) = state.write_vault(&name, &body, next) {
        eprintln!("write_vault({}): {}", name, e);
        return (StatusCode::INTERNAL_SERVER_ERROR, HeaderMap::new(), Vec::new());
    }

    let mut h = HeaderMap::new();
    h.insert(
        "x-vault-version",
        HeaderValue::from_str(&next.to_string()).unwrap(),
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

fn vault_name_for(file: &Path, explicit: Option<&str>) -> String {
    if let Some(n) = explicit {
        return n.to_string();
    }
    file.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "vault".into())
}

fn vault_url(server: &str, name: &str) -> String {
    format!("{}/vaults/{}", server.trim_end_matches('/'), name)
}

// ---------- client commands ----------

async fn cmd_push(file: &Path, server: &str, vault_name: Option<&str>) -> Result<()> {
    let blob = fs::read(file).with_context(|| format!("reading {}", file.display()))?;
    let local_version = read_version(file);
    let name = vault_name_for(file, vault_name);

    let client = reqwest::Client::new();
    let resp = client
        .put(vault_url(server, &name))
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
            println!(
                "Pushed '{}'. Server is now at version {}.",
                name, server_version
            );
            Ok(())
        }
        reqwest::StatusCode::CONFLICT => {
            bail!(
                "Conflict on '{}': server is at version {}; your local state is {}.\n\
                 Pull first to update, then retry.",
                name,
                server_version,
                local_version
            );
        }
        s => bail!("unexpected status {} (server version {})", s, server_version),
    }
}

async fn cmd_pull(file: &Path, server: &str, vault_name: Option<&str>) -> Result<()> {
    let name = vault_name_for(file, vault_name);
    let client = reqwest::Client::new();
    let resp = client.get(vault_url(server, &name)).send().await?;

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
            println!(
                "Pulled '{}' version {} into {}",
                name,
                server_version,
                file.display()
            );
            Ok(())
        }
        reqwest::StatusCode::NOT_FOUND => {
            bail!("server has no vault named '{}' yet — push from another device first", name);
        }
        s => bail!("unexpected status {} (server version {})", s, server_version),
    }
}

use vault_core::{self as vault, Vault};

async fn cmd_merge(file: &Path, server: &str, vault_name: Option<&str>) -> Result<()> {
    let password = rpassword::prompt_password("Master password: ")?;
    let name = vault_name_for(file, vault_name);

    // Load local vault
    let local = vault::load_vault(&file.to_string_lossy(), &password)
        .with_context(|| format!("loading local vault {}", file.display()))?;

    let local_version = read_version(file);

    // Fetch remote blob
    let client = reqwest::Client::new();
    let resp = client.get(vault_url(server, &name)).send().await?;

    let server_version: u64 = resp
        .headers()
        .get("x-vault-version")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        println!("Server has no vault '{}' — pushing local.", name);
    }

    let remote: Vault = if resp.status() == reqwest::StatusCode::OK {
        let bytes = resp.bytes().await?;
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
        .put(vault_url(server, &name))
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
                "Merged '{}' (local v{} + remote v{}), pushed. Server now at version {}.",
                name, local_version, server_version, new_server_version
            );
            Ok(())
        }
        reqwest::StatusCode::CONFLICT => {
            bail!("race: server moved during merge, retry")
        }
        s => bail!("push failed: {}", s),
    }
}
