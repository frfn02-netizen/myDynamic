use std::collections::HashMap;
use std::thread;

use futures_lite::StreamExt;
use tracing::{debug, warn};
use zbus::message::Type;
use zbus::{Connection, MatchRule, Message, MessageStream, OwnedVariant};

use crate::core::{AppEvent, EventSender, NotificationStatus};

const NOTIFICATIONS_SERVICE: &str = "org.freedesktop.Notifications";
const NOTIFICATIONS_PATH: &str = "/org/freedesktop/Notifications";
const NOTIFICATIONS_INTERFACE: &str = "org.freedesktop.Notifications";
const MIN_DISPLAY_MS: u64 = 3_000;
const MAX_DISPLAY_MS: u64 = 12_000;
const DEFAULT_DISPLAY_MS: u64 = 6_000;

type NotifyArgs = (
    String,
    u32,
    String,
    String,
    String,
    Vec<String>,
    HashMap<String, OwnedVariant>,
    i32,
);

#[derive(Clone)]
struct NotificationRecord {
    status: NotificationStatus,
    replaces_id: u32,
}

pub fn spawn(tx: EventSender) {
    let result = thread::Builder::new()
        .name("glass-island-notifications".into())
        .spawn(move || {
            if let Err(error) = zbus::block_on(run(tx)) {
                warn!(%error, "notification service stopped");
            }
        });

    if let Err(error) = result {
        warn!(%error, "failed to start notification service");
    }
}

async fn run(tx: EventSender) -> zbus::Result<()> {
    let connection = Connection::session().await?;

    let notify_rule = MatchRule::builder()
        .msg_type(Type::MethodCall)
        .interface(NOTIFICATIONS_INTERFACE)
        .member("Notify")
        .path(NOTIFICATIONS_PATH)
        .build()?;
    let closed_rule = MatchRule::builder()
        .msg_type(Type::Signal)
        .interface(NOTIFICATIONS_INTERFACE)
        .member("NotificationClosed")
        .path(NOTIFICATIONS_PATH)
        .build()?;
    let return_rule = MatchRule::builder()
        .msg_type(Type::MethodReturn)
        .sender(NOTIFICATIONS_SERVICE)
        .build()?;

    let rules = [
        notify_rule.to_string(),
        closed_rule.to_string(),
        return_rule.to_string(),
    ];
    let become_monitor = connection
        .call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus.Monitoring"),
            "BecomeMonitor",
            &(rules.as_slice(), 0u32),
        )
        .await;

    if let Err(error) = become_monitor {
        warn!(%error, "unable to become a notification monitor; notifications disabled");
        return Ok(());
    }

    debug!("passive notification monitor active");
    let mut stream = MessageStream::from(connection);

    let mut pending: HashMap<u32, NotificationRecord> = HashMap::new();
    let mut current_id: Option<u32> = None;
    let mut latest_serial = 0;
    let mut displayed: Option<NotificationStatus> = None;

    while let Some(message) = stream.next().await {
        let Ok(message) = message else {
            continue;
        };
        let header = message.header();
        match header.message_type() {
            Type::MethodCall => {
                if header.interface().map(|name| name.as_str()) != Some(NOTIFICATIONS_INTERFACE)
                    || header.member().map(|name| name.as_str()) != Some("Notify")
                {
                    continue;
                }
                let Some(record) = parse_notify(&message) else {
                    debug!("failed to parse notification Notify call");
                    continue;
                };
                let serial = header.primary().serial_num().get();
                latest_serial = serial;
                if record.replaces_id != 0 {
                    current_id = Some(record.replaces_id);
                } else {
                    current_id = None;
                }
                pending.insert(serial, record.clone());
                displayed = Some(record.status.clone());
                debug!(
                    app = %record.status.app,
                    summary = %record.status.summary,
                    replaces_id = record.replaces_id,
                    "notification observed"
                );
                let _ = tx.send(AppEvent::Notification(Some(record.status)));
            }
            Type::MethodReturn => {
                let Some(reply_serial) = header.reply_serial().map(|serial| serial.get()) else {
                    continue;
                };
                let Some(record) = pending.remove(&reply_serial) else {
                    continue;
                };
                let Ok(id) = message.body().deserialize::<u32>() else {
                    continue;
                };
                if reply_serial == latest_serial {
                    current_id = Some(id);
                }
                debug!(id, "notification id assigned");
                let _ = record;
            }
            Type::Signal => {
                if header.interface().map(|name| name.as_str()) != Some(NOTIFICATIONS_INTERFACE)
                    || header.member().map(|name| name.as_str()) != Some("NotificationClosed")
                {
                    continue;
                }
                let Ok((id, reason)) = message.body().deserialize::<(u32, u32)>() else {
                    continue;
                };
                if current_id == Some(id) {
                    debug!(id, reason, "notification closed");
                    current_id = None;
                    displayed = None;
                    let _ = tx.send(AppEvent::Notification(None));
                } else if displayed.is_none() {
                    debug!(id, reason, "ignored close for an untracked notification");
                }
            }
            Type::Error => {}
        }
    }

    if displayed.is_some() {
        let _ = tx.send(AppEvent::Notification(None));
    }
    Ok(())
}

fn parse_notify(message: &Message) -> Option<NotificationRecord> {
    let (app, replaces_id, _icon, summary, body, _actions, _hints, expire_timeout): NotifyArgs =
        message.body().deserialize().ok()?;

    let display_ms = notification_display_ms(expire_timeout);
    Some(NotificationRecord {
        status: NotificationStatus {
            app: if app.is_empty() {
                "Application".to_string()
            } else {
                app
            },
            summary,
            body,
            display_ms,
        },
        replaces_id,
    })
}

fn notification_display_ms(expire_timeout: i32) -> u64 {
    if expire_timeout > 0 {
        (expire_timeout as u64).clamp(MIN_DISPLAY_MS, MAX_DISPLAY_MS)
    } else {
        DEFAULT_DISPLAY_MS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_timeout_is_bounded() {
        assert_eq!(notification_display_ms(0), DEFAULT_DISPLAY_MS);
        assert_eq!(notification_display_ms(-1), DEFAULT_DISPLAY_MS);
        assert_eq!(notification_display_ms(1), MIN_DISPLAY_MS);
        assert_eq!(notification_display_ms(5_000), 5_000);
        assert_eq!(notification_display_ms(60_000), MAX_DISPLAY_MS);
    }
}
