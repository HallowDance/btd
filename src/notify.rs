//! Desktop notifications over the session D-Bus.

use std::collections::HashMap;

use anyhow::Result;
use zbus::{Connection, Variant, proxy};

#[proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
trait Notifications {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: HashMap<&str, Variant<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;
}

/// One notification bubble that later messages update in place.
pub struct Notifier {
    proxy: NotificationsProxy<'static>,
    id: u32,
}

impl Notifier {
    pub async fn new() -> Result<Self> {
        let conn = Connection::session().await?;
        Ok(Self {
            proxy: NotificationsProxy::new(&conn).await?,
            id: 0,
        })
    }

    pub async fn show(&mut self, body: &str, error: bool) -> Result<()> {
        let mut hints = HashMap::new();
        // Urgency: 1 normal, 2 critical.
        hints.insert("urgency", Variant::from(if error { 2u8 } else { 1u8 }));
        self.id = self
            .proxy
            .notify(
                "btd",
                self.id,
                "bluetooth",
                "Bluetooth",
                body,
                &[],
                hints,
                5000,
            )
            .await?;
        Ok(())
    }
}
