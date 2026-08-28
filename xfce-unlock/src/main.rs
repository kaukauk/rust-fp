#![warn(unused_crate_dependencies)]

//! Touch-to-unlock for XFCE.
//!
//! XFCE has a single PAM stack for `xfce4-screensaver` and no separate
//! fingerprint service like KDE's `kde-fingerprint`, so the PAM module would sit
//! in the password path and block it. Instead of authenticating through PAM at
//! all, this daemon watches the screensaver, matches a finger against the
//! enrolled templates, and asks logind to unlock the session. The password entry
//! keeps working untouched the whole time.

use std::error::Error;
use std::os::unix::fs::MetadataExt;
use std::time::Duration;

use log::{error, info, warn};
use postcard::from_bytes;
use rust_fp::fingerprint_driver::{MatchOutput, MatchedOutput};
use rust_fp_common::get_templates::get_templates;
use rust_fp_common::rust_fp_dbus::RustFpProxy;
use rust_fp_common::set_templates::set_templates;
use simple_logger::SimpleLogger;
use zbus::export::futures_util::StreamExt;
use zbus::zvariant::OwnedObjectPath;
use zbus::{proxy, Connection};

#[proxy(
    interface = "org.xfce.ScreenSaver",
    default_service = "org.xfce.ScreenSaver",
    default_path = "/org/xfce/ScreenSaver"
)]
trait ScreenSaver {
    fn get_active(&self) -> zbus::Result<bool>;

    #[zbus(signal)]
    fn active_changed(&self, active: bool) -> zbus::Result<()>;
}

#[proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
trait LogindManager {
    /// (session_id, uid, user_name, seat_id, object_path)
    fn list_sessions(&self) -> zbus::Result<Vec<(String, u32, String, String, OwnedObjectPath)>>;

    fn unlock_session(&self, session_id: &str) -> zbus::Result<()>;
}

#[proxy(
    interface = "org.freedesktop.login1.Session",
    default_service = "org.freedesktop.login1"
)]
trait LogindSession {
    #[zbus(property)]
    fn class(&self) -> zbus::Result<String>;

    #[zbus(property, name = "Type")]
    fn session_type(&self) -> zbus::Result<String>;
}

/// Find this user's graphical login session.
///
/// `XDG_SESSION_ID` is deliberately not used: under `systemd --user` it resolves
/// to the manager session (class `manager`), not the seat session that the
/// screensaver locks.
async fn find_graphical_session(system_bus: &Connection) -> Result<String, Box<dyn Error>> {
    let uid = std::fs::metadata("/proc/self")?.uid();
    let manager = LogindManagerProxy::new(system_bus).await?;
    for (id, session_uid, _user, _seat, path) in manager.list_sessions().await? {
        if session_uid != uid {
            continue;
        }
        let session = LogindSessionProxy::builder(system_bus)
            .path(path)?
            .build()
            .await?;
        let class = session.class().await.unwrap_or_default();
        let session_type = session.session_type().await.unwrap_or_default();
        if class == "user" && (session_type == "x11" || session_type == "wayland") {
            return Ok(id);
        }
    }
    Err("No graphical logind session found for this user".into())
}

#[async_std::main]
async fn main() -> Result<(), Box<dyn Error>> {
    SimpleLogger::new()
        .with_level(log::LevelFilter::Info)
        .init()?;

    let session_bus = Connection::session().await?;
    let system_bus = Connection::system().await?;

    let screensaver = ScreenSaverProxy::new(&session_bus).await?;
    let logind = LogindManagerProxy::new(&system_bus).await?;
    let fingerprint = RustFpProxy::new(&system_bus).await?;

    let session_id = find_graphical_session(&system_bus).await?;
    info!("Watching logind session {session_id}");

    let mut active_changed = screensaver.receive_active_changed().await?;
    let mut active = screensaver.get_active().await.unwrap_or(false);

    loop {
        // Only arm the sensor once the screen is actually locked. The driver
        // serialises calls, so an always-armed match would block `rust-fp add`
        // from ever enrolling a new finger.
        while !active {
            let signal = active_changed
                .next()
                .await
                .ok_or("ActiveChanged signal stream ended")?;
            active = signal.args()?.active;
        }
        info!("Screen locked. Waiting for a finger.");

        loop {
            let templates = match get_templates().await {
                Ok(templates) => templates,
                Err(e) => {
                    error!("Could not read templates: {e}. Retrying in 5s.");
                    async_std::task::sleep(Duration::from_secs(5)).await;
                    break;
                }
            };
            if templates.is_empty() {
                warn!("No fingerprints enrolled. Run `rust-fp add <name>`.");
                active = false;
                break;
            }

            // Fix an index order; the daemon maps the matched index back to a
            // label through this same Vec.
            let entries = templates.iter().collect::<Vec<_>>();
            let payload = entries
                .iter()
                .map(|(_label, template)| template.to_vec())
                .collect::<Vec<_>>();

            // Resolves only when a finger touches the sensor. If the screen is
            // unlocked by password in the meantime this stays pending, which is
            // intentional: should the screen be locked again before the next
            // touch, that touch still unlocks it.
            let output = match fingerprint.match_templates(payload).await {
                Ok(bytes) => match from_bytes::<MatchOutput>(&bytes) {
                    Ok(output) => output,
                    Err(e) => {
                        error!("Could not decode match output: {e}. Retrying in 2s.");
                        async_std::task::sleep(Duration::from_secs(2)).await;
                        break;
                    }
                },
                Err(e) => {
                    error!("Match failed: {e}. Retrying in 2s.");
                    async_std::task::sleep(Duration::from_secs(2)).await;
                    break;
                }
            };

            match output {
                MatchOutput::Match(MatchedOutput {
                    index,
                    updated_template,
                }) => {
                    let label = entries[index].0.clone();

                    // The sensor refines a template on a good match; persist it
                    // so recognition keeps improving across reboots.
                    if let Some(updated_template) = updated_template {
                        let mut templates = templates.clone();
                        templates.insert(label.clone(), updated_template);
                        if let Err(e) = set_templates(&templates).await {
                            warn!("Could not save the updated template: {e}");
                        }
                    }

                    // Re-check rather than trust `active`: the match may have
                    // been sitting pending across a password unlock.
                    if screensaver.get_active().await.unwrap_or(false) {
                        match logind.unlock_session(&session_id).await {
                            Ok(()) => info!("Matched {label}. Unlocked session {session_id}."),
                            Err(e) => error!("Matched {label} but unlocking failed: {e}"),
                        }
                    } else {
                        info!("Matched {label}, but the screen is already unlocked. Ignoring.");
                    }
                    active = false;
                    break;
                }
                MatchOutput::NoMatch(reason) => {
                    info!("No match ({reason:?}).");
                    // A hardware-level error can return instantly; back off so a
                    // persistent fault cannot spin the loop.
                    if reason.is_some() {
                        async_std::task::sleep(Duration::from_millis(500)).await;
                    }
                    active = screensaver.get_active().await.unwrap_or(active);
                    if !active {
                        break;
                    }
                }
            }
        }
    }
}
