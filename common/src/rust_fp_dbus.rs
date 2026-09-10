use std::time::{Duration, Instant};

use log::info;
use log::warn;
use postcard::to_allocvec;
use rand::random;
use rust_fp::fingerprint_driver::EnrollStepOutput;
use rust_fp::fingerprint_driver::MatchOutput;
use rust_fp::fingerprint_driver::OpenedFingerprintDriver;
use zbus::{fdo, interface};

use crate::enroll_step_dbus_result::EnrollStepDbusOutput;

/// How long one `enroll_step` waits for a finger before handing the sensor back.
///
/// zbus holds a write lock on this object for the whole of a `&mut self` method,
/// so a wait that never ends blocks every other call — matching included. Short
/// steps keep the interface responsive; the client simply calls again.
const ENROLL_STEP_TIMEOUT: Duration = Duration::from_secs(2);

/// The same, for matching. The lock screen can be dismissed by password while a
/// match is still pending, and that pending call used to keep the sensor for
/// itself, so enrolling afterwards blocked forever.
const MATCH_STEP_TIMEOUT: Duration = Duration::from_secs(2);

/// How long an enrolment may go unpolled before someone else may take the sensor.
///
/// A client killed mid-enrolment — the settings panel closed on the progress
/// dialog, say — stops calling. Without this the sensor stayed reserved for it
/// until the daemon restarted, which took fingerprint unlock down with it.
const ENROLL_ABANDONED_AFTER: Duration = Duration::from_secs(10);

pub struct Enrolling {
    id: u32,
    /// When the owning client last called `enroll_step`.
    last_step: Instant,
}

pub struct RustFp {
    pub driver: Box<dyn OpenedFingerprintDriver>,
    pub enrolling: Option<Enrolling>,
}

impl RustFp {
    /// Drop an enrolment whose client has stopped calling, so the sensor can be
    /// used again. Returns `true` if one was cleared.
    async fn clear_abandoned_enrolment(&mut self) -> bool {
        let Some(enrolling) = self.enrolling.as_ref() else {
            return false;
        };
        if enrolling.last_step.elapsed() < ENROLL_ABANDONED_AFTER {
            return false;
        }
        warn!(
            "Enrolment {} went {:?} without a step. Assuming its client is gone and taking the sensor back.",
            enrolling.id,
            enrolling.last_step.elapsed()
        );
        self.enrolling = None;
        if let Err(e) = self.driver.cancel_enroll().await {
            warn!("Error cancelling the abandoned enrolment: {e:?}");
        }
        true
    }
}

/// Log an enrolment result without dumping the whole template into the journal.
fn describe(result: &Result<EnrollStepOutput, rust_fp::fingerprint_driver::EnrollStepError>) -> String {
    match result {
        Ok(EnrollStepOutput::InProgress(percentage)) => format!("InProgress({percentage}%)"),
        Ok(EnrollStepOutput::Complete(template)) => {
            format!("Complete({} byte template)", template.len())
        }
        Ok(EnrollStepOutput::TimedOut) => "TimedOut".to_owned(),
        Err(e) => format!("{e:?}"),
    }
}

#[interface(
    name = "org.rust_fp.RustFp",
    proxy(
        default_path = "/org/rust_fp/RustFp",
        default_service = "org.rust_fp.RustFp"
    )
)]
impl RustFp {
    async fn get_max_templates(&mut self) -> fdo::Result<u64> {
        let max_templates = self
            .driver
            .get_max_templates()
            .map_err(|_e| fdo::Error::Failed("Error getting max templates".into()))?;
        Ok(max_templates as u64)
    }

    /// A human-readable dump of the sensor and host-command limits.
    async fn get_info(&mut self) -> fdo::Result<String> {
        self.driver
            .describe()
            .map_err(|e| fdo::Error::Failed(format!("{e:?}")))
    }

    async fn enroll_step(&mut self, id: u32) -> fdo::Result<Vec<u8>> {
        self.clear_abandoned_enrolment().await;
        let id = match self.enrolling.as_ref() {
            None => random(),
            Some(enrolling) if enrolling.id == id => enrolling.id,
            Some(_) => {
                return Err(fdo::Error::Failed(
                    "Something else is in the middle of enrolling. Wait until it's done.".into(),
                ))
            }
        };
        self.enrolling = Some(Enrolling {
            id,
            last_step: Instant::now(),
        });

        let result = self
            .driver
            .start_or_continue_enroll(ENROLL_STEP_TIMEOUT)
            .await;
        match result {
            Ok(EnrollStepOutput::Complete(_)) => self.enrolling = None,
            _ => {
                // Measure the abandonment clock from the end of the wait, so it
                // counts the gap between calls rather than time spent waiting
                // for a finger that simply has not arrived yet.
                if let Some(enrolling) = self.enrolling.as_mut() {
                    enrolling.last_step = Instant::now();
                }
            }
        }
        info!("Enroll id: {id}. Result: {}.", describe(&result));
        Ok(to_allocvec(&EnrollStepDbusOutput { id, result }).unwrap())
    }

    /// Give up an enrolment and put the sensor back to work.
    ///
    /// Safe to call when nothing is enrolling, and safe to call with a stale id.
    async fn cancel_enroll(&mut self, id: u32) -> fdo::Result<()> {
        match self.enrolling.as_ref() {
            Some(enrolling) if enrolling.id == id => {
                self.enrolling = None;
                self.driver
                    .cancel_enroll()
                    .await
                    .map_err(|e| fdo::Error::Failed(format!("{e:?}")))?;
                info!("Enrolment {id} cancelled.");
                Ok(())
            }
            _ => Ok(()),
        }
    }

    async fn match_templates(&mut self, templates: Vec<Vec<u8>>) -> fdo::Result<Vec<u8>> {
        self.clear_abandoned_enrolment().await;
        if let Some(enrolling) = self.enrolling.as_ref() {
            // Enrolling and matching both own the sensor. Say so rather than
            // queueing behind it, so the caller can back off and retry.
            return Err(fdo::Error::Failed(format!(
                "Enrolment {} is in progress",
                enrolling.id
            )));
        }
        let output = self
            .driver
            .match_templates(&templates, MATCH_STEP_TIMEOUT)
            .await
            .map_err(|e| fdo::Error::Failed(format!("{e:?}")))?;
        if !matches!(output, MatchOutput::TimedOut) {
            warn!("Match result: {output:?}");
        }
        Ok(to_allocvec(&output).unwrap())
    }
}
