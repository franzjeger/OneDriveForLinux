//! Automatic first-run setup for OneDrive for Linux.
//!
//! Strategy (in order):
//!
//! 1. **Azure CLI** — if `az` is installed and logged in as an admin, use it to
//!    create the app registration programmatically. No OAuth bootstrapping needed.
//! 2. **Manual fallback** — print a direct Azure Portal link and prompt the user
//!    to paste the client_id after registering the app themselves.
//!
//! After either path, writes `~/.config/onedrive-linux/config.toml`.

use anyhow::{bail, Context, Result};
use std::{path::Path, process::Command};
use tracing::info;

/// Microsoft Graph app ID (constant across all tenants).
const GRAPH_APP_ID: &str = "00000003-0000-0000-c000-000000000000";

/// `Files.ReadWrite.All` delegated permission GUID on Microsoft Graph.
const FILES_RW_ALL: &str = "863451e7-0667-486c-a5d6-d135439485f0";

pub struct AdminSetup;

impl AdminSetup {
    /// Runs the first-run setup and returns the created/entered client_id.
    pub async fn run(config_path: &Path) -> Result<String> {
        println!("\n===== OneDrive for Linux — First-Run Setup =====\n");

        match try_az_cli_setup() {
            Ok((client_id, tenant_id)) => {
                println!("✓ App registration created via Azure CLI");
                println!("  client_id : {client_id}");
                println!("  tenant_id : {tenant_id}");
                write_config(config_path, &client_id, &tenant_id)?;
                println!("\n✓ Config written to {}\n", config_path.display());
                Ok(client_id)
            }
            Err(az_err) => {
                info!("Azure CLI setup unavailable: {az_err:#}");
                manual_setup(config_path).await
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Path 1 — Azure CLI
// ---------------------------------------------------------------------------

fn try_az_cli_setup() -> Result<(String, String)> {
    // Verify az is installed.
    Command::new("az")
        .args(["--version"])
        .output()
        .context("az CLI not found")?;

    // Verify az is logged in.
    let whoami = az(&["account", "show", "--query", "user.name", "-o", "tsv"])?;
    if whoami.trim().is_empty() {
        bail!("az CLI is not logged in — run `az login` first");
    }
    println!("Using Azure CLI (logged in as {})", whoami.trim());

    // Fetch tenant ID.
    let tenant_id = az(&["account", "show", "--query", "tenantId", "-o", "tsv"])?;
    let tenant_id = tenant_id.trim().to_owned();

    // Create the app registration.
    println!("Creating app registration…");
    let create_json = az(&[
        "ad", "app", "create",
        "--display-name", "OneDrive for Linux",
        "--sign-in-audience", "AzureADandPersonalMicrosoftAccount",
        "--is-fallback-public-client", "true",
        "--public-client-redirect-uris",
        "https://login.microsoftonline.com/common/oauth2/nativeclient",
        "--query", "appId",
        "-o", "tsv",
    ])?;
    let client_id = create_json.trim().to_owned();
    if client_id.is_empty() {
        bail!("az returned empty appId");
    }

    // Add Files.ReadWrite.All delegated permission.
    println!("Adding Files.ReadWrite.All permission…");
    az(&[
        "ad", "app", "permission", "add",
        "--id", &client_id,
        "--api", GRAPH_APP_ID,
        "--api-permissions", &format!("{FILES_RW_ALL}=Scope"),
    ])?;

    // Wait for Azure AD to propagate the new app registration before consenting.
    println!("Waiting for Azure AD to propagate app registration…");
    std::thread::sleep(std::time::Duration::from_secs(15));

    // Grant admin consent.
    println!("Granting admin consent…");
    match az(&["ad", "app", "permission", "admin-consent", "--id", &client_id]) {
        Ok(_) => {}
        Err(e) => {
            // Non-fatal: the user may grant consent interactively on first sign-in.
            println!("  (Admin consent skipped — users will be prompted on first sign-in: {e:#})");
        }
    }

    Ok((client_id, tenant_id))
}

/// Runs an `az` command, returns trimmed stdout, or an error with stderr.
fn az(args: &[&str]) -> Result<String> {
    let out = Command::new("az")
        .args(args)
        .output()
        .with_context(|| format!("run: az {}", args.join(" ")))?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("az {}: {}", args.join(" "), stderr.trim());
    }

    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

// ---------------------------------------------------------------------------
// Path 2 — Manual fallback
// ---------------------------------------------------------------------------

async fn manual_setup(config_path: &Path) -> Result<String> {
    println!("Azure CLI is not available or not logged in as an admin.\n");
    println!("Please create an app registration manually (takes ~2 minutes):\n");
    println!("  1. Open: https://portal.azure.com/#view/Microsoft_AAD_RegisteredApps/CreateApplicationBlade");
    println!("  2. Display name: OneDrive for Linux");
    println!("  3. Supported account types: Personal Microsoft accounts only");
    println!("     (or 'Accounts in any organizational directory and personal accounts' for work+personal)");
    println!("  4. Redirect URI: Mobile/Desktop → https://login.microsoftonline.com/common/oauth2/nativeclient");
    println!("  5. After creating: API permissions → Add → Microsoft Graph →");
    println!("     Delegated → Files.ReadWrite.All");
    println!("  6. Copy the Application (client) ID from the Overview page\n");

    // Try to open the portal in the browser (best-effort).
    let _ = std::process::Command::new("xdg-open")
        .arg("https://portal.azure.com/#view/Microsoft_AAD_RegisteredApps/CreateApplicationBlade")
        .spawn();

    let client_id = prompt("Paste your client_id here: ").await?;
    let client_id = client_id.trim().to_owned();
    if client_id.is_empty() {
        bail!("No client_id entered");
    }

    let tenant = prompt("Tenant ID (press Enter for 'common' — personal accounts): ").await?;
    let tenant_id = if tenant.trim().is_empty() {
        "common".to_owned()
    } else {
        tenant.trim().to_owned()
    };

    write_config(config_path, &client_id, &tenant_id)?;
    println!("\n✓ Config written to {}\n", config_path.display());

    Ok(client_id)
}

async fn prompt(msg: &str) -> Result<String> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    print!("{msg}");
    // Flush stdout so the prompt appears before we block on stdin.
    use std::io::Write;
    std::io::stdout().flush().ok();

    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let mut line = String::new();
    reader.read_line(&mut line).await.context("read stdin")?;
    Ok(line)
}

// ---------------------------------------------------------------------------
// Config writer
// ---------------------------------------------------------------------------

fn write_config(config_path: &Path, client_id: &str, tenant_id: &str) -> Result<()> {
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent).context("create config directory")?;
    }

    let sync_dir = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("/root"))
        .join("OneDrive");

    let content = format!(
        "# OneDrive for Linux — auto-generated by first-run setup\n\
         # Edit this file to change sync settings.\n\
         \n\
         client_id = {client_id:?}\n\
         tenant_id = {tenant_id:?}\n\
         sync_dir  = {sync_dir:?}\n",
        client_id = client_id,
        tenant_id = tenant_id,
        sync_dir = sync_dir.to_string_lossy(),
    );

    std::fs::write(config_path, content).context("write config.toml")?;
    Ok(())
}
