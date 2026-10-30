use std::ffi::CString;
use std::io::BufReader;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use pulseaudio::protocol::{
    self, Command, CommandReply, GetSinkInfo, Prop, Props, ServerInfo, SinkInfo, SubscriptionMask,
};
use tracing::{debug, warn};

use crate::core::{AppEvent, EventSender, VolumeStatus};

const CLIENT_NAME: &str = "glass-island";
const REFRESH_INTERVAL: Duration = Duration::from_secs(5);
const RETRY_DELAY: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeControl {
    Expanded(bool),
    Refresh,
}

pub fn spawn(tx: EventSender) -> Sender<VolumeControl> {
    let (control_tx, control_rx) = mpsc::channel();
    let refresh_tx = control_tx.clone();
    let event_tx = tx.clone();

    if let Err(error) = thread::Builder::new()
        .name("glass-island-volume-events".into())
        .spawn(move || run_events(event_tx, refresh_tx))
    {
        warn!(%error, "failed to start volume event service");
    }

    if let Err(error) = thread::Builder::new()
        .name("glass-island-volume-query".into())
        .spawn(move || run_query(tx, control_rx))
    {
        warn!(%error, "failed to start volume query service");
    }

    control_tx
}

struct Connection {
    reader: BufReader<std::os::unix::net::UnixStream>,
    version: u16,
    seq: u32,
}

impl Connection {
    fn connect(suffix: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let socket_path =
            pulseaudio::socket_path_from_env().ok_or("PulseAudio socket is unavailable")?;
        let socket = std::os::unix::net::UnixStream::connect(socket_path)?;
        let mut reader = BufReader::new(socket);

        let cookie = pulseaudio::cookie_path_from_env()
            .and_then(|path| std::fs::read(path).ok())
            .unwrap_or_default();
        let auth = protocol::AuthParams {
            version: protocol::MAX_VERSION,
            supports_shm: false,
            supports_memfd: false,
            cookie,
        };
        protocol::write_command_message(
            reader.get_mut(),
            0,
            &Command::Auth(auth),
            protocol::MAX_VERSION,
        )?;
        let (_, reply) = protocol::read_reply_message::<protocol::AuthReply>(
            &mut reader,
            protocol::MAX_VERSION,
        )?;
        let version = protocol::MAX_VERSION.min(reply.version);

        let client_name = CString::new(format!("{CLIENT_NAME}-{suffix}"))?;
        let mut props = Props::new();
        props.set(Prop::ApplicationName, client_name);
        props.set(
            Prop::ApplicationProcessId,
            CString::new(std::process::id().to_string())?,
        );
        protocol::write_command_message(
            reader.get_mut(),
            1,
            &Command::SetClientName(props),
            version,
        )?;
        let _ = protocol::read_reply_message::<protocol::SetClientNameReply>(&mut reader, version)?;

        Ok(Self {
            reader,
            version,
            seq: 1,
        })
    }

    fn request<T: CommandReply>(
        &mut self,
        command: Command,
    ) -> Result<T, Box<dyn std::error::Error>> {
        self.seq = self.seq.wrapping_add(1);
        protocol::write_command_message(self.reader.get_mut(), self.seq, &command, self.version)?;
        let (reply_seq, reply) = protocol::read_reply_message::<T>(&mut self.reader, self.version)?;
        if reply_seq != self.seq {
            return Err(format!(
                "PulseAudio reply sequence mismatch: expected {}, got {}",
                self.seq, reply_seq
            )
            .into());
        }
        Ok(reply)
    }
}

fn run_events(tx: EventSender, refresh: Sender<VolumeControl>) {
    loop {
        if let Err(error) = run_events_inner(refresh.clone()) {
            warn!(%error, "volume event service unavailable; retrying");
            let _ = tx.send(AppEvent::Volume(None));
        }
        thread::sleep(RETRY_DELAY);
    }
}

fn run_events_inner(refresh: Sender<VolumeControl>) -> Result<(), Box<dyn std::error::Error>> {
    let mut events = Connection::connect("events")?;
    events.seq = events.seq.wrapping_add(1);
    protocol::write_command_message(
        events.reader.get_mut(),
        events.seq,
        &Command::Subscribe(SubscriptionMask::SINK | SubscriptionMask::SERVER),
        events.version,
    )?;
    let ack = protocol::read_ack_message(&mut events.reader)?;
    if ack != events.seq {
        return Err(format!(
            "PulseAudio subscription ack mismatch: expected {}, got {}",
            events.seq, ack
        )
        .into());
    }
    // A previous query connection may have died while this subscription was
    // being re-established.
    let _ = refresh.send(VolumeControl::Refresh);

    loop {
        let (_, command) = protocol::read_command_message(&mut events.reader, events.version)?;
        let Command::SubscribeEvent(event) = command else {
            continue;
        };

        use protocol::{SubscriptionEventFacility as Facility, SubscriptionEventType as EventType};
        // PipeWire's PulseAudio compatibility layer does not emit a server event
        // for some default-sink changes. The query thread rechecks every five
        // seconds while the island is expanded, so stale device state is bounded.
        let relevant_facility = matches!(event.event_facility, Facility::Sink | Facility::Server);
        let relevant_type = matches!(
            event.event_type,
            EventType::Changed | EventType::New | EventType::Removed
        );
        if relevant_facility && relevant_type {
            debug!(
                facility = ?event.event_facility,
                kind = ?event.event_type,
                index = event.index,
                "PulseAudio subscription event"
            );
            let _ = refresh.send(VolumeControl::Refresh);
        }
    }
}

fn run_query(tx: EventSender, control: Receiver<VolumeControl>) {
    let mut last = None;
    let mut expanded = false;

    loop {
        let mut query = match Connection::connect("query") {
            Ok(connection) => connection,
            Err(error) => {
                warn!(%error, "volume query service unavailable; retrying");
                let _ = tx.send(AppEvent::Volume(None));
                if !wait_for_retry(&control, &mut expanded) {
                    return;
                }
                continue;
            }
        };

        if expanded && let Err(error) = refresh_volume(&mut query, &tx, &mut last) {
            warn!(%error, "failed to refresh output volume");
            let _ = tx.send(AppEvent::Volume(None));
            if !wait_for_retry(&control, &mut expanded) {
                return;
            }
            continue;
        }

        loop {
            if !expanded {
                match control.recv() {
                    Ok(VolumeControl::Expanded(value)) => expanded = value,
                    Ok(VolumeControl::Refresh) => {}
                    Err(_) => return,
                }
                if expanded && let Err(error) = refresh_volume(&mut query, &tx, &mut last) {
                    warn!(%error, "failed to refresh output volume");
                    break;
                }
                continue;
            }

            match control.recv_timeout(REFRESH_INTERVAL) {
                Ok(VolumeControl::Expanded(false)) => expanded = false,
                Ok(VolumeControl::Expanded(true)) | Ok(VolumeControl::Refresh) => {
                    if let Err(error) = refresh_volume(&mut query, &tx, &mut last) {
                        warn!(%error, "failed to refresh output volume");
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    if let Err(error) = refresh_volume(&mut query, &tx, &mut last) {
                        warn!(%error, "failed to refresh output volume");
                        break;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }

        let _ = tx.send(AppEvent::Volume(None));
        if !wait_for_retry(&control, &mut expanded) {
            return;
        }
    }
}

fn wait_for_retry(control: &Receiver<VolumeControl>, expanded: &mut bool) -> bool {
    let deadline = Instant::now() + RETRY_DELAY;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return true;
        }
        match control.recv_timeout(remaining) {
            Ok(VolumeControl::Expanded(value)) => *expanded = value,
            Ok(VolumeControl::Refresh) => {}
            Err(RecvTimeoutError::Timeout) => return true,
            Err(RecvTimeoutError::Disconnected) => return false,
        }
    }
}

fn refresh_volume(
    query: &mut Connection,
    tx: &EventSender,
    last: &mut Option<VolumeStatus>,
) -> Result<(), Box<dyn std::error::Error>> {
    emit_current(query, tx, last)
}

fn channel_volume_level(channels: &[pulseaudio::protocol::Volume]) -> f32 {
    const NORMAL_VOLUME: f32 = 65536.0;
    if channels.is_empty() {
        return 0.0;
    }
    channels
        .iter()
        .map(|volume| volume.as_u32() as f32 / NORMAL_VOLUME)
        .sum::<f32>()
        / channels.len() as f32
}

fn emit_current(
    query: &mut Connection,
    tx: &EventSender,
    last: &mut Option<VolumeStatus>,
) -> Result<(), Box<dyn std::error::Error>> {
    let server: ServerInfo = query.request(Command::GetServerInfo)?;
    let Some(sink_name) = server.default_sink_name else {
        if last.take().is_some() {
            let _ = tx.send(AppEvent::Volume(None));
        }
        return Ok(());
    };

    let sink: SinkInfo = query.request(Command::GetSinkInfo(GetSinkInfo {
        index: None,
        name: Some(sink_name.clone()),
    }))?;

    let level = channel_volume_level(sink.cvolume.channels());
    debug!(
        level,
        muted = sink.muted,
        sink = %sink_name.to_string_lossy(),
        "output volume sampled"
    );
    let status = VolumeStatus {
        level,
        muted: sink.muted,
    };

    if last.as_ref() == Some(&status) {
        return Ok(());
    }

    debug!(
        level,
        muted = status.muted,
        sink = %sink_name.to_string_lossy(),
        "output volume updated"
    );
    *last = Some(status);
    let _ = tx.send(AppEvent::Volume(Some(status)));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulseaudio::protocol::Volume;

    #[test]
    fn channel_volume_averages_normalized_values() {
        assert_eq!(channel_volume_level(&[]), 0.0);
        assert_eq!(channel_volume_level(&[Volume::NORM, Volume::NORM]), 1.0);
        assert_eq!(channel_volume_level(&[Volume::MUTED, Volume::MUTED]), 0.0);
        assert!((channel_volume_level(&[Volume::NORM, Volume::MUTED]) - 0.5).abs() < f32::EPSILON);
    }
}
