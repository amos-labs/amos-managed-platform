//! Transactional email via Amazon SES.
//!
//! Config-gated and safe-by-default: a [`Mailer`] only exists when
//! `AMOS__EMAIL__FROM` is set (the verified SES sender). When email is not
//! configured the platform behaves exactly as before — password-reset links are
//! created but not delivered (an admin mints/hands them out instead).

use aws_sdk_sesv2::types::{Body, Content, Destination, EmailContent, Message};
use aws_sdk_sesv2::Client as SesClient;

/// Sends transactional emails through SES. The region comes from the shared
/// AWS config; the sender from `AMOS__EMAIL__FROM`.
pub struct Mailer {
    client: SesClient,
    from: String,
}

impl Mailer {
    /// Build a mailer from the shared AWS config. Returns `None` (email
    /// disabled) unless `AMOS__EMAIL__FROM` is set and non-empty.
    pub fn from_env(aws_config: &aws_config::SdkConfig) -> Option<Self> {
        let from = std::env::var("AMOS__EMAIL__FROM")
            .ok()
            .filter(|s| !s.trim().is_empty())?;
        Some(Self {
            client: SesClient::new(aws_config),
            from,
        })
    }

    /// Send a password-reset email containing the one-time reset link.
    pub async fn send_password_reset(&self, to: &str, reset_url: &str) -> anyhow::Result<()> {
        let subject = Content::builder()
            .data("Reset your AMOS password")
            .charset("UTF-8")
            .build()?;
        let text = Content::builder()
            .data(format!(
                "Use the link below to set a new password. It expires in 24 hours.\n\n{reset_url}\n\nIf you didn't request this, you can ignore this email."
            ))
            .charset("UTF-8")
            .build()?;
        let html = Content::builder()
            .data(format!(
                "<p>Use the button below to set a new password. This link expires in 24 hours.</p>\
                 <p><a href=\"{reset_url}\">Reset your password</a></p>\
                 <p>If the button doesn't work, paste this URL into your browser:<br>{reset_url}</p>\
                 <p style=\"color:#888\">If you didn't request this, you can ignore this email.</p>"
            ))
            .charset("UTF-8")
            .build()?;

        let body = Body::builder().text(text).html(html).build();
        let message = Message::builder().subject(subject).body(body).build();
        let content = EmailContent::builder().simple(message).build();
        let destination = Destination::builder().to_addresses(to).build();

        self.client
            .send_email()
            .from_email_address(&self.from)
            .destination(destination)
            .content(content)
            .send()
            .await?;
        Ok(())
    }
}
