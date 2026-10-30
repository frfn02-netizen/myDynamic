use std::collections::HashMap;
use std::path::Path;
use std::thread;
use std::time::Duration;

use futures_lite::{StreamExt, future};
use tracing::{debug, warn};
use zbus::{Message, OwnedVariant, Proxy};

use crate::core::{AppEvent, EventSender, MediaStatus};

const DBUS_SERVICE: &str = "org.freedesktop.DBus";
const DBUS_PATH: &str = "/org/freedesktop/DBus";
const DBUS_INTERFACE: &str = "org.freedesktop.DBus";
const MPRIS_PREFIX: &str = "org.mpris.MediaPlayer2.";
const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";
const MPRIS_PLAYER: &str = "org.mpris.MediaPlayer2.Player";
const RETRY_DELAY: Duration = Duration::from_secs(5);

pub fn spawn(tx: EventSender) {
    let result = thread::Builder::new()
        .name("glass-island-mpris".into())
        .spawn(move || run(tx));

    if let Err(error) = result {
        warn!(%error, "failed to start MPRIS media service");
    }
}

fn run(tx: EventSender) {
    loop {
        match zbus::block_on(run_once(tx.clone())) {
            Ok(()) => debug!("MPRIS connection ended; reconnecting"),
            Err(error) => warn!(%error, "MPRIS media service unavailable; retrying"),
        }
        let _ = tx.send(AppEvent::Media(None));
        thread::sleep(RETRY_DELAY);
    }
}

enum Wake {
    Player(Option<Message>),
    Names(bool),
}

async fn run_once(tx: EventSender) -> zbus::Result<()> {
    let connection = zbus::Connection::session().await?;
    let dbus = Proxy::new(&connection, DBUS_SERVICE, DBUS_PATH, DBUS_INTERFACE).await?;
    let mut names = dbus.receive_signal("NameOwnerChanged").await?;
    // Start with a sentinel so the UI receives an explicit initial state.
    let mut last: Option<MediaStatus> = Some(MediaStatus::default());

    loop {
        let players = list_players(&dbus).await;
        let Some(player) = select_player(&connection, &players).await else {
            if last.take().is_some() {
                debug!("no MPRIS player; clearing media");
                let _ = tx.send(AppEvent::Media(None));
            }
            if !next_interesting_name_change(&mut names).await {
                return Ok(());
            }
            continue;
        };

        debug!(%player.destination, "MPRIS player selected");
        emit_player(&player.proxy, &player.destination, &tx, &mut last).await;

        let mut properties = match player.properties.receive_signal("PropertiesChanged").await {
            Ok(stream) => stream,
            Err(error) => {
                debug!(%error, "failed to subscribe to MPRIS properties");
                if last.take().is_some() {
                    debug!("MPRIS properties unavailable; clearing media");
                    let _ = tx.send(AppEvent::Media(None));
                }
                if !next_interesting_name_change(&mut names).await {
                    return Ok(());
                }
                continue;
            }
        };

        loop {
            let wake = future::race(async { Wake::Player(properties.next().await) }, async {
                Wake::Names(next_interesting_name_change(&mut names).await)
            })
            .await;

            match wake {
                Wake::Player(Some(message)) => {
                    if mpris_properties_changed(&message) {
                        emit_player(&player.proxy, &player.destination, &tx, &mut last).await;
                    }
                }
                Wake::Player(None) => {
                    if last.take().is_some() {
                        debug!("MPRIS properties stream ended; clearing media");
                        let _ = tx.send(AppEvent::Media(None));
                    }
                    break;
                }
                Wake::Names(true) => break,
                Wake::Names(false) => return Ok(()),
            }
        }
    }
}

struct SelectedPlayer {
    destination: String,
    proxy: Proxy<'static>,
    properties: Proxy<'static>,
}

async fn list_players(dbus: &Proxy<'_>) -> Vec<String> {
    let reply = match dbus.call_method("ListNames", &()).await {
        Ok(reply) => reply,
        Err(error) => {
            debug!(%error, "failed to list D-Bus names");
            return Vec::new();
        }
    };

    reply
        .body()
        .deserialize::<Vec<String>>()
        .unwrap_or_default()
        .into_iter()
        .filter(|name| name.starts_with(MPRIS_PREFIX))
        .collect()
}

async fn select_player(
    connection: &zbus::Connection,
    players: &[String],
) -> Option<SelectedPlayer> {
    let mut best: Option<(u8, SelectedPlayer)> = None;

    for destination in players {
        let Ok(proxy) = Proxy::new_owned(
            connection.clone(),
            destination.clone(),
            MPRIS_PATH.to_string(),
            MPRIS_PLAYER.to_string(),
        )
        .await
        else {
            continue;
        };

        let playing = proxy
            .get_property::<String>("PlaybackStatus")
            .await
            .map(|status| status == "Playing")
            .unwrap_or(false);
        let metadata = proxy
            .get_property::<HashMap<String, OwnedVariant>>("Metadata")
            .await
            .unwrap_or_default();
        let title = metadata_string(&metadata, "xesam:title");
        let score = u8::from(playing) * 4
            + u8::from(!title.is_empty()) * 2
            + u8::from(!metadata.is_empty());

        if best
            .as_ref()
            .is_some_and(|(best_score, _)| score <= *best_score)
        {
            continue;
        }

        let Ok(properties) = Proxy::new_owned(
            connection.clone(),
            destination.clone(),
            MPRIS_PATH.to_string(),
            "org.freedesktop.DBus.Properties".to_string(),
        )
        .await
        else {
            continue;
        };

        best = Some((
            score,
            SelectedPlayer {
                destination: destination.clone(),
                proxy,
                properties,
            },
        ));

        if score >= 6 {
            break;
        }
    }

    best.map(|(_, player)| player)
}

async fn next_interesting_name_change(names: &mut zbus::proxy::SignalStream<'_>) -> bool {
    loop {
        let Some(message) = names.next().await else {
            return false;
        };
        let Ok((name, _, _)) = message.body().deserialize::<(String, String, String)>() else {
            continue;
        };
        if name.starts_with(MPRIS_PREFIX) {
            return true;
        }
    }
}

fn mpris_properties_changed(message: &Message) -> bool {
    let Ok((_, changed, invalidated)) =
        message
            .body()
            .deserialize::<(String, HashMap<String, OwnedVariant>, Vec<String>)>()
    else {
        return true;
    };

    ["Metadata", "PlaybackStatus"]
        .iter()
        .any(|key| changed.contains_key(*key) || invalidated.iter().any(|name| name == key))
}

async fn emit_player(
    proxy: &Proxy<'_>,
    name: &str,
    tx: &EventSender,
    last: &mut Option<MediaStatus>,
) {
    let playing = proxy
        .get_property::<String>("PlaybackStatus")
        .await
        .map(|status| status == "Playing")
        .unwrap_or(false);
    let metadata = proxy
        .get_property::<HashMap<String, OwnedVariant>>("Metadata")
        .await
        .unwrap_or_default();

    let title = metadata_string(&metadata, "xesam:title");
    let status = if title.is_empty() {
        None
    } else {
        let artist = metadata
            .get("xesam:artist")
            .and_then(|value| value.try_clone().ok())
            .and_then(|value| Vec::<String>::try_from(value).ok())
            .map(|artists| artists.join(", "))
            .unwrap_or_default();
        let length_us = metadata.get("mpris:length").and_then(|value| {
            if let Some(value) = value.try_clone().ok().and_then(|v| u64::try_from(v).ok()) {
                return Some(value);
            }
            value
                .try_clone()
                .ok()
                .and_then(|v| i64::try_from(v).ok())
                .and_then(|v| u64::try_from(v).ok())
        });
        let art_path = file_url_to_path(&metadata_string(&metadata, "mpris:artUrl"));
        let position_us = proxy
            .get_property::<i64>("Position")
            .await
            .ok()
            .and_then(|value| u64::try_from(value).ok());

        Some(MediaStatus {
            title,
            artist,
            playing,
            length_us,
            position_us,
            art_path,
        })
    };

    if last.as_ref() == status.as_ref() {
        return;
    }

    if let Some(status) = &status {
        debug!(%name, title = %status.title, artist = %status.artist, playing, "MPRIS metadata updated");
    }

    *last = status.clone();
    let _ = tx.send(AppEvent::Media(status));
}

fn metadata_string(metadata: &HashMap<String, OwnedVariant>, key: &str) -> String {
    metadata
        .get(key)
        .and_then(|value| value.try_clone().ok())
        .and_then(|value| String::try_from(value).ok())
        .unwrap_or_default()
}

fn file_url_to_path(url: &str) -> Option<String> {
    let rest = url.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    if !rest.starts_with('/') {
        return None;
    }

    let bytes = rest.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return None;
            }
            let high = hex_digit(bytes[index + 1])?;
            let low = hex_digit(bytes[index + 2])?;
            decoded.push(high << 4 | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }

    let path = String::from_utf8(decoded).ok()?;
    Path::new(&path).is_file().then_some(path)
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_file_urls_are_decoded() {
        let path =
            std::env::temp_dir().join(format!("glass island art test {}.png", std::process::id()));
        std::fs::write(&path, b"not really an image").expect("write temp file");
        let encoded = path.to_string_lossy().replace(' ', "%20");
        let url = format!("file://{encoded}");
        assert_eq!(
            file_url_to_path(&url),
            Some(path.to_string_lossy().into_owned())
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn remote_and_missing_urls_are_ignored() {
        assert_eq!(file_url_to_path("https://example.test/art.png"), None);
        assert_eq!(file_url_to_path("file:///definitely/not/here.png"), None);
        assert_eq!(file_url_to_path("not-a-url"), None);
    }
}
