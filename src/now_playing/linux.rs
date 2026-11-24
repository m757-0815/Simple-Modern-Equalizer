use super::Track;
use std::{collections::HashMap, time::Duration};
use zbus::{blocking::{Connection, Proxy, connection::Builder}, zvariant::OwnedValue};

#[derive(Default)]
pub(super) struct Source {
    connection: Option<Connection>,
}

type Properties = HashMap<String, OwnedValue>;

impl Source {
    pub(super) fn track(&mut self) -> Option<Track> {
        if self.connection.is_none() {
            self.connection = Builder::session().and_then(|builder| {
                builder.method_timeout(Duration::from_secs(1)).build()
            }).ok();
        }
        match self.connection.as_ref().map(current_track) {
            Some(Ok(track)) => track,
            _ => {
                self.connection = None;
                None
            }
        }
    }
}

fn current_track(connection: &Connection) -> zbus::Result<Option<Track>> {
    let bus = Proxy::new(connection, "org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus")?;
    let players: Vec<String> = bus.call("ListNames", &())?;
    Ok(players.iter().filter(|name| name.starts_with("org.mpris.MediaPlayer2."))
        .find_map(|name| read_track(connection, name)))
}

fn read_track(connection: &Connection, name: &str) -> Option<Track> {
    let player = Proxy::new(connection, name, "/org/mpris/MediaPlayer2", "org.freedesktop.DBus.Properties").ok()?;
    let mut properties: Properties = player.call("GetAll", &("org.mpris.MediaPlayer2.Player",)).ok()?;
    if <&str>::try_from(properties.get("PlaybackStatus")?).ok()? != "Playing" {
        return None;
    }
    let mut metadata = Properties::try_from(properties.remove("Metadata")?).ok()?;
    let title = String::try_from(metadata.remove("xesam:title")?).ok()?;
    if title.trim().is_empty() {
        return None;
    }
    let artist = metadata.remove("xesam:artist")
        .and_then(|value| Vec::<String>::try_from(value).ok()).unwrap_or_default().join(", ");
    Some(Track { title, artist })
}
