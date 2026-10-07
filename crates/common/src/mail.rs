//! Notices to builders and reporters, sent from the platform mailbox.

use lettre::message::{Mailbox, header::ContentType};
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

pub struct Mailer {
    from: Mailbox,
    transport: AsyncSmtpTransport<Tokio1Executor>,
}

impl Mailer {
    /// `url`: `smtps://user:pass@smtp.gmail.com:465`, `smtp://relay:587?tls=required`
    /// or `smtp://127.0.0.1:2525` (plain, local tests only).
    pub fn new(url: &str, from: &str) -> Result<Self, String> {
        let transport = AsyncSmtpTransport::<Tokio1Executor>::from_url(url)
            .map_err(|e| e.to_string())?
            .build();
        Ok(Self {
            from: from
                .parse()
                .map_err(|e: lettre::address::AddressError| e.to_string())?,
            transport,
        })
    }

    pub async fn send(&self, to: &str, subject: &str, body: &str) -> Result<(), String> {
        let msg = Message::builder()
            .from(self.from.clone())
            .to(to
                .parse()
                .map_err(|e: lettre::address::AddressError| e.to_string())?)
            .subject(subject)
            .header(ContentType::TEXT_PLAIN)
            .body(body.to_string())
            .map_err(|e| e.to_string())?;
        self.transport
            .send(msg)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}
