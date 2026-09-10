use futures::future::BoxFuture;
use std::error::Error;
use std::io;
use std::time::Duration;

type OpenAndInit =
    Box<dyn Fn() -> BoxFuture<'static, io::Result<Box<dyn OpenedFingerprintDriver>>>>;

pub struct FingerprintDriver {
    /// Returns `true` if this device has a  fingerprint sensor compatible with this driver
    pub is_compatible: Box<dyn Fn() -> BoxFuture<'static, io::Result<bool>>>,
    pub name: &'static str,
    pub open_and_init: OpenAndInit,
}

#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug)]
pub enum EnrollStepError {
    GenericError,
    LowQuality,
}

#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug)]
pub enum EnrollStepOutput {
    InProgress(u8),
    Complete(Vec<u8>),
    /// The deadline passed with no finger on the sensor. The enrolment is still
    /// open and keeps the progress it had; call `start_or_continue_enroll`
    /// again to keep waiting.
    TimedOut,
}

pub type EnrollStepResult = Result<EnrollStepOutput, EnrollStepError>;

#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug)]
pub struct MatchedOutput {
    pub index: usize,
    /// If the template was updated successfully, the updated template should be outputted
    pub updated_template: Option<Vec<u8>>,
}

#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum NoMatchError {
    Other,
    LowQuality,
}

#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug)]
pub enum MatchOutput {
    Match(MatchedOutput),
    NoMatch(Option<NoMatchError>),
    /// The deadline passed with no finger on the sensor. Nothing was decided;
    /// call `match_templates` again to keep waiting.
    TimedOut,
}

/// Every wait a driver performs is bounded.
///
/// The sensor can only do one thing at a time, so whoever is waiting on it
/// holds it. An unbounded wait means a caller that goes away — a settings panel
/// that was closed mid-enrolment, a lock screen dismissed by password — keeps
/// the sensor to itself until the daemon is restarted, which takes fingerprint
/// unlock down with it. Bounded waits let the holder be preempted; callers that
/// still want to wait simply call again.
pub trait OpenedFingerprintDriver: Sync + Send {
    /// Run one enrolment step, waiting at most `timeout` for a finger.
    fn start_or_continue_enroll(&mut self, timeout: Duration) -> BoxFuture<'_, EnrollStepResult>;
    /// Throw away a partial enrolment and leave the sensor ready to match again.
    ///
    /// Safe to call when no enrolment is open.
    fn cancel_enroll(&mut self) -> BoxFuture<'_, anyhow::Result<()>>;
    fn get_max_templates(&mut self) -> anyhow::Result<usize>;
    /// A human-readable dump of the sensor and host-command limits, for bug
    /// reports from hardware the author cannot test on.
    fn describe(&mut self) -> anyhow::Result<String>;
    /// Wait at most `timeout` for a finger to match one of `templates`.
    fn match_templates<'a>(
        &'a mut self,
        templates: &'a [Vec<u8>],
        timeout: Duration,
    ) -> BoxFuture<'a, Result<MatchOutput, Box<dyn Error>>>;
}
