use anyhow::{anyhow, Context, Result};
use colored::*;
use std::io::{self, Write};

use crate::config::{load_global_config, AuthMethod};
use crate::oauth2;

/// Run the OAuth2 device code flow for an account and cache the token.
pub async fn cmd_oauth2_login(account_name: Option<&str>) -> Result<()> {
    let config = load_global_config()?;

    // Resolve which account to use
    let account = if let Some(name) = account_name {
        config
            .accounts
            .iter()
            .find(|a| a.name == name)
            .ok_or_else(|| anyhow!("Account '{}' not found in config", name))?
    } else {
        // Find the first OAuth2 or Graph account, or first account
        config
            .accounts
            .iter()
            .find(|a| a.auth_method == AuthMethod::OAuth2 || a.auth_method == AuthMethod::Graph)
            .or_else(|| config.accounts.first())
            .ok_or_else(|| anyhow!("No accounts configured"))?
    };

    if account.auth_method != AuthMethod::OAuth2 && account.auth_method != AuthMethod::Graph {
        return Err(anyhow!(
            "Account '{}' uses auth_method = \"password\", not \"oauth2\" or \"graph\". \
             Set auth_method = \"oauth2\" or \"graph\" in config.toml to use OAuth2.",
            account.name
        ));
    }

    let oauth2_settings = account.oauth2.as_ref().ok_or_else(|| {
        anyhow!(
            "Account '{}' requires an [accounts.oauth2] section with client_id and tenant_id.",
            account.name
        )
    })?;

    if oauth2_settings.client_id.is_empty() || oauth2_settings.tenant_id.is_empty() {
        return Err(anyhow!(
            "OAuth2 client_id and tenant_id must be set for account '{}'",
            account.name
        ));
    }

    let scopes = oauth2::scopes_for_auth_method(&account.auth_method);

    println!(
        "{} Starting OAuth2 device code flow for account '{}' ({})",
        "\u{2139}".blue(),
        account.name,
        if account.auth_method == AuthMethod::Graph { "Graph API" } else { "IMAP/SMTP" },
    );

    let cache = oauth2::device_code_flow(
        &oauth2_settings.client_id,
        &oauth2_settings.tenant_id,
        &account.name,
        scopes,
    ).await?;

    println!(
        "{} OAuth2 token acquired and cached for account '{}'",
        "\u{2713}".green(),
        account.name
    );

    if account.auth_method == AuthMethod::Graph {
        // Graph accounts: test with a Graph API call instead of IMAP/SMTP
        print!("  Testing Graph API connection... ");
        io::stdout().flush()?;

        match test_graph_api(&cache.access_token).await {
            Ok((display_name, mail)) => {
                println!("{}", "OK".green());
                println!(
                    "  {} Authenticated as: {} <{}>",
                    "\u{2713}".green(),
                    display_name,
                    mail,
                );
            }
            Err(e) => {
                println!("{}", "FAILED".red());
                eprintln!(
                    "  {} Graph API test failed: {}",
                    "\u{26a0}".yellow(),
                    e
                );
                eprintln!("  The token was cached, but the Graph API rejected the request.");
                eprintln!("  Possible causes:");
                eprintln!("    - The app registration may need admin consent for Mail.Read / Mail.Send");
                eprintln!("    - The account may not have an Exchange Online mailbox");
            }
        }
    } else {
        // OAuth2 accounts: test IMAP + SMTP connections
        print!("  Testing IMAP connection... ");
        io::stdout().flush()?;

        let imap_config = crate::config::ImapConfig {
            host: if account.imap.host.is_empty() {
                account.smtp.host.clone()
            } else {
                account.imap.host.clone()
            },
            port: account.imap.port,
            username: if account.imap.username.is_empty() {
                account.smtp.username.clone()
            } else {
                account.imap.username.clone()
            },
            password: cache.access_token.clone(),
            accept_invalid_certs: account.imap.accept_invalid_certs,
            auth_method: AuthMethod::OAuth2,
            fetch_concurrency: 1,
        };

        match async {
            let mut session = crate::imap_client::open_imap_session(&imap_config).await?;
            session.logout().await.ok();
            Ok::<_, anyhow::Error>(())
        }.await {
            Ok(()) => println!("{}", "OK".green()),
            Err(e) => {
                println!("{}", "FAILED".red());
                eprintln!(
                    "  {} IMAP connection failed: {}",
                    "\u{26a0}".yellow(),
                    e
                );
                eprintln!("  The token was cached, but the server rejected the connection.");
                eprintln!("  Possible causes:");
                eprintln!("    - IMAP may be disabled for your tenant (contact IT)");
                eprintln!("    - The app registration may need admin consent");
                eprintln!("    - The IMAP host/port may be incorrect");
            }
        }

        // Test SMTP connection
        print!("  Testing SMTP connection... ");
        io::stdout().flush()?;

        let smtp_config = crate::config::SmtpConfig {
            host: account.smtp.host.clone(),
            port: account.smtp.port,
            username: account.smtp.username.clone(),
            password: cache.access_token,
            default_from: account.default_from.clone(),
            accept_invalid_certs: account.smtp.accept_invalid_certs,
            auth_method: AuthMethod::OAuth2,
        };

        match test_smtp_oauth2(&smtp_config) {
            Ok(()) => println!("{}", "OK".green()),
            Err(e) => {
                println!("{}", "FAILED".red());
                eprintln!(
                    "  {} SMTP connection failed: {}",
                    "\u{26a0}".yellow(),
                    e
                );
                eprintln!("  SMTP sending may not work. Check SMTP host/port and permissions.");
            }
        }
    }

    Ok(())
}

/// Test the Graph API connection by calling /me and returning (display_name, mail).
async fn test_graph_api(access_token: &str) -> Result<(String, String)> {
    #[derive(serde::Deserialize)]
    struct MeResponse {
        #[serde(default, rename = "displayName")]
        display_name: String,
        #[serde(default)]
        mail: String,
    }

    let client = reqwest::Client::new();
    let resp = client
        .get("https://graph.microsoft.com/v1.0/me")
        .bearer_auth(access_token)
        .send()
        .await
        .context("Failed to call Graph API /me endpoint")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("Graph API returned HTTP {}: {}", status, body));
    }

    let me: MeResponse = resp.json().await.context("Failed to parse /me response")?;
    let mail = if me.mail.is_empty() {
        "(no mail address)".to_string()
    } else {
        me.mail
    };
    Ok((me.display_name, mail))
}

/// Test SMTP connection with OAuth2 credentials.
fn test_smtp_oauth2(smtp_config: &crate::config::SmtpConfig) -> Result<()> {
    use lettre::transport::smtp::authentication::{Credentials, Mechanism};

    if smtp_config.accept_invalid_certs {
        crate::config::ensure_invalid_certs_allowed(&smtp_config.host)?;
    }
    let creds = Credentials::new(smtp_config.username.clone(), smtp_config.password.clone());

    let mailer = if smtp_config.port == 465 {
        let mut transport = lettre::SmtpTransport::relay(&smtp_config.host)?;
        if smtp_config.accept_invalid_certs {
            let tls_params =
                lettre::transport::smtp::client::TlsParameters::builder(smtp_config.host.clone())
                    .dangerous_accept_invalid_certs(true)
                    .build()?;
            transport =
                transport.tls(lettre::transport::smtp::client::Tls::Wrapper(tls_params));
        }
        transport
            .port(smtp_config.port)
            .credentials(creds)
            .authentication(vec![Mechanism::Xoauth2])
            .build()
    } else {
        let mut transport = lettre::SmtpTransport::starttls_relay(&smtp_config.host)?;
        if smtp_config.accept_invalid_certs {
            let tls_params =
                lettre::transport::smtp::client::TlsParameters::builder(smtp_config.host.clone())
                    .dangerous_accept_invalid_certs(true)
                    .build()?;
            transport =
                transport.tls(lettre::transport::smtp::client::Tls::Required(tls_params));
        }
        transport
            .port(smtp_config.port)
            .credentials(creds)
            .authentication(vec![Mechanism::Xoauth2])
            .build()
    };

    mailer.test_connection().context("SMTP OAuth2 connection test failed")?;
    Ok(())
}
