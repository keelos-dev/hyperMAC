//! The RDP display: the Mac's primary screen, full frames from capture.rs; IronRDP's
//! encoder keeps its own framebuffer and sends only what changed (RemoteFX or bitmaps).

use async_trait::async_trait;
use ironrdp_server::{DesktopSize, DisplayUpdate, RdpServerDisplay, RdpServerDisplayUpdates, ServerResult};
use tracing::warn;

use crate::capture::{self, Screen};

pub struct Display {
    screen: Screen,
    /// keelOS::Video channel for this connection, when offered (video.rs): the capture sends
    /// the picture as H.264 through it instead of as bitmaps while the client runs a stream.
    video: Option<crate::video::Video>,
    /// RDPSND of this connection (sound.rs); the capture's audio output feeds it.
    sound: Option<crate::sound::Sound>,
    /// "mute the Mac while remote" (tap.rs), independent of the sound switch
    mute: crate::tap::MuteSwitch,
}

impl Display {
    pub fn new(
        screen: Screen,
        video: Option<crate::video::Video>,
        sound: Option<crate::sound::Sound>,
        mute: crate::tap::MuteSwitch,
    ) -> Self {
        Self { screen, video, sound, mute }
    }
}

#[async_trait]
impl RdpServerDisplay for Display {
    async fn size(&mut self) -> DesktopSize {
        DesktopSize {
            width: self.screen.width.min(u32::from(u16::MAX)) as u16,
            height: self.screen.height.min(u32::from(u16::MAX)) as u16,
        }
    }

    async fn updates(&mut self) -> ServerResult<Box<dyn RdpServerDisplayUpdates>> {
        // A capture that fails to start (permission revoked, display gone) ends the
        // session instead of erroring the server: the channel closes, next_update
        // returns None.
        let rx = match capture::start(self.screen, self.video.clone(), self.sound.clone(), self.mute.clone()) {
            Ok(rx) => rx,
            Err(e) => {
                warn!("capture: {e:#}");
                let (_tx, rx) = tokio::sync::mpsc::channel(1);
                rx
            }
        };
        // Injected input is not "user activity" to macOS: after displaysleep (10 min by
        // default) the display went dark under a live session (black frames, 2026-11-09).
        // caffeinate -d holds a PreventUserIdleDisplaySleep assertion while this session
        // lasts, -u declares activity now (wakes a display that already slept).
        let awake = std::process::Command::new("/usr/bin/caffeinate")
            .args(["-d", "-u"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| warn!("caffeinate: {e} (the display may sleep under the session)"))
            .ok();
        // the pointer as the client's own (cursor.rs); the capture leaves it out of the picture
        let cursor = Some(crate::cursor::start());
        Ok(Box::new(Updates { rx, cursor, awake }))
    }
}

struct Updates {
    rx: tokio::sync::mpsc::Receiver<DisplayUpdate>,
    /// pointer shapes (cursor.rs); None once that thread is gone
    cursor: Option<tokio::sync::mpsc::Receiver<DisplayUpdate>>,
    /// `caffeinate -d -u` for the life of the session (see updates()).
    awake: Option<std::process::Child>,
}

impl Drop for Updates {
    fn drop(&mut self) {
        if let Some(c) = &mut self.awake {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

#[async_trait]
impl RdpServerDisplayUpdates for Updates {
    async fn next_update(&mut self) -> ServerResult<Option<DisplayUpdate>> {
        // Dropping Updates drops rx; the capture thread sees the closed channel and stops
        // (and the cursor thread likewise).
        loop {
            let Some(cursor) = &mut self.cursor else {
                return Ok(self.rx.recv().await);
            };
            tokio::select! {
                frame = self.rx.recv() => return Ok(frame),
                shape = cursor.recv() => match shape {
                    Some(update) => return Ok(Some(update)),
                    None => self.cursor = None,
                },
            }
        }
    }
}
