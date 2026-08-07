use anyhow::{anyhow, Context, Result};
use std::io::{self, Write};

/// Block on a future from sync context.
///
/// `mp config init` and friends are sync but reachable from `#[tokio::main]`,
/// so calling `Runtime::new()` directly panics with "Cannot start a runtime
/// from within a runtime". Detect that case and run the future on a fresh
/// runtime in a dedicated OS thread.
pub(crate) fn run_async_blocking<F, T>(fut: F) -> Result<T>
where
    F: std::future::Future<Output = Result<T>> + Send + 'static,
    T: Send + 'static,
{
    if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::scope(|s| {
            s.spawn(|| {
                let rt = tokio::runtime::Runtime::new()?;
                rt.block_on(fut)
            })
            .join()
            .map_err(|_| anyhow!("async work thread panicked"))?
        })
    } else {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(fut)
    }
}

/// Prompt user for text input with an optional default value.
pub(crate) fn prompt_input(prompt: &str, default: &str) -> Result<String> {
    if default.is_empty() {
        print!("{}: ", prompt);
    } else {
        print!("{} [{}]: ", prompt, default);
    }
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let trimmed = input.trim();

    if trimmed.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(trimmed.to_string())
    }
}

/// Prompt user to select a mailbox from a list using FuzzySelect.
/// Pre-selects the first matching candidate from `preferred`.
pub(crate) fn select_mailbox(prompt: &str, options: &[String], preferred: &[&str]) -> Result<String> {
    let default_idx = preferred
        .iter()
        .find_map(|pref| {
            options
                .iter()
                .position(|o| o.eq_ignore_ascii_case(pref))
        })
        .unwrap_or(0);

    let selection = dialoguer::FuzzySelect::new()
        .with_prompt(prompt)
        .items(options)
        .default(default_idx)
        .interact()
        .context("Selection cancelled")?;

    Ok(options[selection].clone())
}

/// Test SMTP connection: connect with TLS and authenticate.
pub(crate) fn test_smtp_connection(host: &str, port: u16, username: &str, password: &str, accept_invalid_certs: bool) -> Result<()> {
    use lettre::transport::smtp::authentication::Credentials;

    if accept_invalid_certs {
        crate::config::ensure_invalid_certs_allowed(host)?;
    }
    let creds = Credentials::new(username.to_string(), password.to_string());

    let mailer = if port == 465 {
        let mut transport = lettre::SmtpTransport::relay(host)?;
        if accept_invalid_certs {
            let tls_params = lettre::transport::smtp::client::TlsParameters::builder(host.to_string())
                .dangerous_accept_invalid_certs(true)
                .build()?;
            transport = transport.tls(lettre::transport::smtp::client::Tls::Wrapper(tls_params));
        }
        transport.port(port).credentials(creds).build()
    } else {
        let mut transport = lettre::SmtpTransport::starttls_relay(host)?;
        if accept_invalid_certs {
            let tls_params = lettre::transport::smtp::client::TlsParameters::builder(host.to_string())
                .dangerous_accept_invalid_certs(true)
                .build()?;
            transport = transport.tls(lettre::transport::smtp::client::Tls::Required(tls_params));
        }
        transport.port(port).credentials(creds).build()
    };

    mailer.test_connection()?;
    Ok(())
}

/// Test IMAP connection: connect with TLS, login, and logout.
pub(crate) fn test_imap_connection(host: &str, port: u16, username: &str, password: &str, accept_invalid_certs: bool) -> Result<()> {
    use crate::imap_client::open_imap_session;

    let imap_config = crate::config::ImapConfig {
        host: host.to_string(),
        port,
        username: username.to_string(),
        password: password.to_string(),
        accept_invalid_certs,
        auth_method: crate::config::AuthMethod::Password,
        fetch_concurrency: 1,
    };

    run_async_blocking(async move {
        let mut session = open_imap_session(&imap_config).await?;
        session.logout().await.ok();
        Ok(())
    })
}
