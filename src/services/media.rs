use std::collections::HashMap;
use std::thread;

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

pub fn spawn(tx: EventSender) {
    let result = thread::Builder::new()
        .name("glass-island-mpris".into())
        .spawn(move || {
            if let Err(error) = zbus::block_on(run(tx)) {
                warn!(%error, "MPRIS media service stopped");
            }
        });

    if let Err(error) = result {
        warn!(%error, "failed to start MPRIS media service");
    }
}

enum Wake {
    Player(Option<Message>),
    Names(bool),
}

async fn run(tx: EventSender) -> zbus::Result<()> {
    let connection = zbus::Connection::session().await?;
    let dbus = Proxy::new(&connection, DBUS_SERVICE, DBUS_PATH, DBUS_INTERFACE).await?;
    let mut names = dbus.receive_signal("NameOwnerChanged").await?;
    // Start with a sentinel so the UI receives an explicit initial state.
    let mut last: Option<MediaStatus> = Some(MediaStatus::default());

    loop {
        let players = list_players(&dbus).await;
        let Some(player) = select_player(&connection, &players).await else {
            if last.take().is_some() {
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

        Some(MediaStatus {
            title,
            artist,
            playing,
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
