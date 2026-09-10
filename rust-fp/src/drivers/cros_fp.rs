use std::{
    collections::HashSet,
    error::Error,
    hash::{DefaultHasher, Hash, Hasher},
    io::{self, ErrorKind},
    os::fd::AsRawFd,
    time::{Duration, Instant},
};

use async_std::{fs::File, task::sleep, task::spawn_blocking};
use crosec::{
    commands::{
        fp_download::fp_download_template,
        fp_get_encryption_status::{fp_get_encryption_status, FpEncryptionStatus},
        fp_info::{fp_info, EcResponseFpInfo},
        fp_mode::{fp_mode, FpMode},
        fp_set_context::fp_set_context,
        fp_set_seed::fp_set_seed,
        get_protocol_info::{get_protocol_info, EcResponseGetProtocolInfo},
        get_uptime_info::ec_cmd_get_uptime_info,
        CrosEcCmd,
    },
    ec_command::ec_command_with_dynamic_output_size,
    wait_event::{
        event::{EcMkbpEvent, EcMkbpEventType},
        fingerprint::{
            EcMkbpEventFingerprintEnrollError, EcMkbpEventFingerprintMatchResult,
            EcMkbpEventFingerprintNoMatchError, EcMkbpEventFingerprintRust,
        },
        host_event::HostEventCode,
        wait_event_async,
    },
    CROS_EC_IOC_MAGIC, CROS_FP_PATH,
};
use futures::future::BoxFuture;

use crate::drivers::GetFingerprintDriver;
use crate::fingerprint_driver::{
    EnrollStepError, EnrollStepOutput, EnrollStepResult, FingerprintDriver, MatchOutput,
    MatchedOutput, NoMatchError, OpenedFingerprintDriver,
};

/// `_IO(CROS_EC_IOC_MAGIC, 2)` — tells the chardev which MKBP events should make
/// this file description readable. crosec issues it inside `wait_event_async`,
/// but that call then blocks until an event arrives; we issue it ourselves so we
/// can poll instead.
const CROS_EC_DEV_IOCEVENTMASK: libc::c_ulong = ((CROS_EC_IOC_MAGIC as libc::c_ulong) << 8) | 2;

/// How long to wait for the finger to come off the sensor after an enrolment
/// image. Generous on purpose: it only has to cover a slow lift, and giving up
/// early merely risks the double count it exists to prevent.
const FINGER_UP_TIMEOUT: Duration = Duration::from_secs(5);

fn event_mask<I: IntoIterator<Item = EcMkbpEventType>>(event_types: I) -> i32 {
    let mut mask = i32::default();
    for event_type in event_types {
        mask |= 1 << event_type as u8;
    }
    mask
}

/// Wait for one of `event_types`, giving up after `timeout`.
///
/// `wait_event_async` waits forever, and dropping its future would not cancel
/// the read async-std has already handed to its blocking pool — that read would
/// still be sitting there to swallow the next touch. So arm the mask, wait on
/// the descriptor with `poll`, and only read once an event is actually queued.
/// Giving up is then free, because nothing is ever left half-read.
///
/// The `poll` runs on a blocking thread rather than being retried on a timer:
/// the sensor sits armed for as long as the screen is locked, and a polling
/// loop would wake the CPU for the whole of it.
async fn wait_event_timeout<I>(
    file: &mut File,
    event_types: I,
    timeout: Duration,
) -> io::Result<Option<EcMkbpEvent>>
where
    I: IntoIterator<Item = EcMkbpEventType> + Clone,
{
    let fd = file.as_raw_fd();
    unsafe {
        libc::ioctl(
            fd,
            CROS_EC_DEV_IOCEVENTMASK,
            event_mask(event_types.clone()),
        )
    };
    let timeout_ms = timeout.as_millis().min(i32::MAX as u128) as libc::c_int;
    let (ready, revents) = spawn_blocking(move || {
        let mut poll_fd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: Default::default(),
        };
        let ready = unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) };
        (ready, poll_fd.revents)
    })
    .await;
    if ready < 0 {
        return Err(io::Error::last_os_error());
    }
    if revents & libc::POLLIN == 0 {
        // Timed out with nothing queued.
        return Ok(None);
    }
    // An event is queued, so this read returns straight away.
    Ok(Some(wait_event_async(file, event_types).await?))
}

/// Size of `struct ec_params_fp_template`'s header: a `u32` offset and a `u32`
/// size, followed by the template bytes.
const FP_TEMPLATE_HEADER_SIZE: usize = 8;

/// Flag in the `size` field marking the chunk that completes the template.
const FP_TEMPLATE_COMMIT: u32 = 0x80000000;

/// Send one template to the sensor.
///
/// This exists because crosec's own `fp_upload_template` sizes its chunks from
/// `max_ec_output_size()` — the EC-to-host response limit — while counting them
/// with `max_ec_input_size()`, the host-to-EC request limit. Most CrOS hardware
/// reports the same figure for both (544/544) and the mix-up is invisible, but
/// this FPMCU reports 544 in and 256 out. The loop then sent ten 236-byte
/// chunks and set the "template complete" flag after 2360 of 5156 bytes; the EC
/// tried to decrypt a truncated template and answered UNAVAILABLE. That is why
/// an enrolled finger worked until the first restart and never again: matching
/// only re-uploads once the sensor has been cleared, and the upload could not
/// succeed.
///
/// A template travels host-to-EC, so both the chunk size and the chunk count
/// come from the request limit.
fn upload_template<F: AsRawFd>(
    file: &mut F,
    protocol_info: &EcResponseGetProtocolInfo,
    fp_info: &EcResponseFpInfo,
    template: &[u8],
) -> Result<(), Box<dyn Error>> {
    if template.len() != fp_info.template_size as usize {
        return Err(format!(
            "Template is {} bytes but this sensor's templates are {}",
            template.len(),
            fp_info.template_size
        )
        .into());
    }
    // TODO(b/78544921): dropping 32 bits is a workaround for an MCU bug. The
    // ChromiumOS ectool drops 4 bytes here too.
    let max_chunk_size = protocol_info.max_ec_input_size() - FP_TEMPLATE_HEADER_SIZE - 4;
    let number_of_chunks = template.len().div_ceil(max_chunk_size);
    for chunk_index in 0..number_of_chunks {
        let offset = chunk_index * max_chunk_size;
        let size = (template.len() - offset).min(max_chunk_size);
        let mut params = Vec::with_capacity(FP_TEMPLATE_HEADER_SIZE + size);
        params.extend_from_slice(&(offset as u32).to_le_bytes());
        params.extend_from_slice(&{
            let mut flagged = size as u32;
            if chunk_index == number_of_chunks - 1 {
                flagged |= FP_TEMPLATE_COMMIT;
            }
            flagged
        }
        .to_le_bytes());
        params.extend_from_slice(&template[offset..offset + size]);
        ec_command_with_dynamic_output_size(
            CrosEcCmd::FpTemplate,
            0,
            &params,
            0,
            file.as_raw_fd(),
        )
        .map_err(|e| format!("Error uploading template chunk {chunk_index}: {e:?}"))?;
    }
    Ok(())
}

pub struct CrosFp;

impl GetFingerprintDriver for CrosFp {
    fn get_driver() -> FingerprintDriver {
        FingerprintDriver {
            name: "Chromebook",
            is_compatible: Box::new(|| {
                Box::pin(async {
                    match File::open(CROS_FP_PATH).await {
                        Ok(_file) => Ok(true),
                        Err(e) => match e.kind() {
                            ErrorKind::NotFound => Ok(false),
                            _ => Err(e),
                        },
                    }
                })
            }),
            open_and_init: Box::new(|| {
                Box::pin(async {
                    let opened_cros_fp = OpenedCrosFp::open_and_init().await?;
                    Ok(Box::new(opened_cros_fp) as Box<dyn OpenedFingerprintDriver>)
                })
            }),
        }
    }
}

pub struct OpenedCrosFp {
    file: File,
    protocol_info: EcResponseGetProtocolInfo,
    loaded_templates_hashes: Vec<u64>,
    fp_info: EcResponseFpInfo,
    /// Whether an enrolment session is open on the sensor.
    enroll_open: bool,
}

impl OpenedCrosFp {
    async fn open_and_init() -> io::Result<Self> {
        let mut file = File::open(CROS_FP_PATH).await?;
        fp_mode(&mut file, FpMode::Reset as u32).unwrap();
        fp_mode(&mut file, FpMode::ResetSensor as u32).unwrap();
        let protocol_info = get_protocol_info(&mut file).unwrap();
        let fp_info = fp_info(&mut file).unwrap();
        Ok(Self {
            file,
            loaded_templates_hashes: Default::default(),
            protocol_info,
            fp_info,
            enroll_open: false,
        })
    }

    async fn wait_until_duration_after_fpmcu_boot(&mut self, duration: Duration) {
        let uptime_info = ec_cmd_get_uptime_info(&mut self.file).unwrap();
        if let Some(sleep_duration) = duration.checked_sub(Duration::from_millis(
            uptime_info.time_since_ec_boot_ms.into(),
        )) {
            sleep(sleep_duration).await;
        }
    }

    /// Sets the seed if it hasn't been set. Clears hashes if seed got un-set,
    /// which indicates that the FP sensor restarted, which happens during suspend.
    async fn ensure_seed_is_set(&mut self) {
        // This number was found through trial and error. 1250 failed, but there may be some room for improvement after even more trial and error.
        self.wait_until_duration_after_fpmcu_boot(Duration::from_millis(1350))
            .await;
        let status = fp_get_encryption_status(&mut self.file).unwrap();
        if status.status & (FpEncryptionStatus::SeedSet as u32) == 0 {
            // Set the seed to "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" so that it can easily be typed manually too.
            // I'm pretty sure the tradition of using this seed was made by WeirdTreeThing.
            fp_set_seed(&mut self.file, [b'a'; 32]).unwrap();
            self.loaded_templates_hashes.clear();
        }
    }

    /// Sets the context. The context must be set before enrolling and uploading
    /// The context gets reset when the sensor gets reset
    /// It is possible that the seed doesn't get reset but the context does
    fn set_context(&mut self) {
        fp_set_context(&mut self.file, [0xaa; 32]).unwrap();
        // Setting context always clears templates, even if the context was previously set to the same value
        self.loaded_templates_hashes.clear();
    }

    /// Get the sensor into a state where the context can be set, then set it.
    ///
    /// `FpMode::from_repr` only recognises single flags, so deciding this by
    /// converting the mode panicked on any combination — including
    /// `EnrollSession | EnrollImage`, which is exactly what an abandoned
    /// enrolment leaves behind. Test the bits instead.
    fn ensure_context_set(&mut self) -> anyhow::Result<()> {
        let mode = fp_mode(&mut self.file, FpMode::DontChange as u32)
            .map_err(|e| anyhow::anyhow!("Error reading fp mode: {e:?}"))?;
        if mode & FpMode::EnrollSession as u32 != 0 {
            // Mid-enrolment: the context is already set, and cannot be set again.
            return Ok(());
        }
        if mode != FpMode::Reset as u32 {
            // The context can only be set from Reset.
            fp_mode(&mut self.file, FpMode::Reset as u32)
                .map_err(|e| anyhow::anyhow!("Error resetting fp mode: {e:?}"))?;
        }
        self.set_context();
        Ok(())
    }

    fn check_if_templates_got_cleared(&mut self) {
        if self.loaded_templates_hashes.is_empty() {
            return;
        }
        let Ok(info) = fp_info(&mut self.file) else {
            self.loaded_templates_hashes.clear();
            return;
        };
        if info.template_valid != self.loaded_templates_hashes.len() as u16 {
            // Either the sensor dropped them (suspend, a reset) or an abandoned
            // enrolment left a partial template behind. Either way what we
            // believe is loaded is wrong, so forget it and let the next match
            // upload from scratch. This used to panic, which killed the daemon
            // and took screen unlocking down with it.
            self.loaded_templates_hashes.clear();
        }
    }

    /// Take the sensor out of enrolment and forget what we think it holds.
    fn reset_enroll(&mut self) -> anyhow::Result<()> {
        self.enroll_open = false;
        let result = fp_mode(&mut self.file, FpMode::Reset as u32);
        // A half-finished enrolment leaves a partial template in the sensor, so
        // the next match has to re-upload rather than trust the loaded set.
        self.loaded_templates_hashes.clear();
        result
            .map(|_mode| ())
            .map_err(|e| anyhow::anyhow!("Error resetting fp mode: {e:?}"))
    }

    /// Wait for the finger to leave the sensor.
    ///
    /// The FPMCU will happily capture the finger still resting on it, so
    /// re-arming `EnrollImage` straight after an image counted one press twice:
    /// progress jumped 20% then 40% on a single touch and reached 100% in two or
    /// three presses, on images that were nearly identical. Waiting for the lift
    /// makes one press mean one step.
    ///
    /// `EnrollSession` stays in the mask because `fp_mode` sets the mode
    /// outright rather than or-ing into it — dropping the flag here would end
    /// the enrolment and lose its progress.
    async fn wait_for_finger_up(&mut self) {
        if fp_mode(
            &mut self.file,
            FpMode::EnrollSession as u32 | FpMode::FingerUp as u32,
        )
        .is_err()
        {
            return;
        }
        let deadline = Instant::now() + FINGER_UP_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return;
            }
            match wait_event_timeout(&mut self.file, [EcMkbpEventType::Fingerprint], remaining).await
            {
                Ok(Some(EcMkbpEvent::Fingerprint(event))) => {
                    if matches!(event.rust(), EcMkbpEventFingerprintRust::FingerUp) {
                        return;
                    }
                }
                // Timed out, or an error. The next step re-arms the capture
                // anyway; a missed lift costs at worst the double count this is
                // here to avoid.
                _ => return,
            }
        }
    }
}

impl OpenedFingerprintDriver for OpenedCrosFp {
    fn start_or_continue_enroll(&mut self, timeout: Duration) -> BoxFuture<'_, EnrollStepResult> {
        Box::pin(async move {
            self.ensure_seed_is_set().await;
            if !self.enroll_open {
                self.check_if_templates_got_cleared();
                // Set the context if the sensor is full (which clears it) or if
                // nothing is loaded, since then it may not be set at all.
                if self.loaded_templates_hashes.len() == self.fp_info.template_max as usize
                    || self.loaded_templates_hashes.is_empty()
                {
                    self.ensure_context_set()
                        .map_err(|_e| EnrollStepError::GenericError)?;
                }
            }

            // The FPMCU clears EnrollImage once it has captured one, so re-arm
            // only when it is not already armed. Re-arming every time round the
            // timeout loop would ask for a fresh image on each pass.
            let mode = fp_mode(&mut self.file, FpMode::DontChange as u32)
                .map_err(|_e| EnrollStepError::GenericError)?;
            if mode & FpMode::EnrollImage as u32 == 0 {
                fp_mode(
                    &mut self.file,
                    FpMode::EnrollSession as u32 | FpMode::EnrollImage as u32,
                )
                .map_err(|_e| EnrollStepError::GenericError)?;
            }
            self.enroll_open = true;

            let event =
                wait_event_timeout(&mut self.file, [EcMkbpEventType::Fingerprint], timeout)
                    .await
                    .map_err(|_e| EnrollStepError::GenericError)?;
            let Some(event) = event else {
                // Nobody touched the sensor. Hand it back so anything else
                // waiting on the driver gets a turn; the enrolment stays open.
                return Ok(EnrollStepOutput::TimedOut);
            };
            let data = match event {
                EcMkbpEvent::Fingerprint(event) => match event.rust() {
                    EcMkbpEventFingerprintRust::Enroll(output) => Ok(output),
                    _ => Err(EnrollStepError::GenericError),
                },
                _ => Err(EnrollStepError::GenericError),
            }?;

            if let Some(error) = data.error {
                // Wait for the lift here too: retrying against the finger still
                // on the sensor would just reproduce the same bad image.
                self.wait_for_finger_up().await;
                // TODO: Add more specific errors. I didn't add any because I never experienced them
                return Err(match error {
                    EcMkbpEventFingerprintEnrollError::LowQuality => EnrollStepError::LowQuality,
                    _ => EnrollStepError::GenericError,
                });
            }

            match data.percentage {
                100 => {
                    let enrolled_template_index = self.loaded_templates_hashes.len();
                    let template = fp_download_template(
                        &mut self.file,
                        &self.fp_info,
                        &self.protocol_info,
                        enrolled_template_index,
                    );
                    let template_vec: Vec<u8> = template.into();
                    let hash = {
                        let mut hasher = DefaultHasher::default();
                        template_vec.hash(&mut hasher);
                        hasher.finish()
                    };
                    self.loaded_templates_hashes.push(hash);
                    self.enroll_open = false;
                    // Close the session. Enrolment used to just stop calling,
                    // which left the sensor sitting in EnrollSession for
                    // whatever ran next.
                    let _ = fp_mode(&mut self.file, FpMode::Reset as u32);
                    Ok(EnrollStepOutput::Complete(template_vec))
                }
                percentage => {
                    self.wait_for_finger_up().await;
                    Ok(EnrollStepOutput::InProgress(percentage))
                }
            }
        })
    }

    fn cancel_enroll(&mut self) -> BoxFuture<'_, anyhow::Result<()>> {
        Box::pin(async move { self.reset_enroll() })
    }

    fn get_max_templates(&mut self) -> anyhow::Result<usize> {
        Ok(self.fp_info.template_max as usize)
    }

    fn describe(&mut self) -> anyhow::Result<String> {
        let info = fp_info(&mut self.file)
            .map_err(|e| anyhow::anyhow!("Error reading fp info: {e:?}"))?;
        let mode = fp_mode(&mut self.file, FpMode::DontChange as u32)
            .map_err(|e| anyhow::anyhow!("Error reading fp mode: {e:?}"))?;
        let encryption = fp_get_encryption_status(&mut self.file)
            .map_err(|e| anyhow::anyhow!("Error reading encryption status: {e:?}"))?;
        let input = self.protocol_info.max_ec_input_size();
        let output = self.protocol_info.max_ec_output_size();
        let chunk = input - FP_TEMPLATE_HEADER_SIZE - 4;
        // The vendor id is four ASCII bytes little-endian: "FPC " for
        // Fingerprint Cards, "ELAN" for Elan.
        let vendor = String::from_utf8_lossy(&info.vendor_id.to_le_bytes()).to_string();
        Ok(format!(
            "Driver:            Chromebook (/dev/cros_fp)\n\
             Sensor vendor:     {:#010x} ({:?})\n\
             Sensor product:    {} model {} version {}\n\
             Image:             {}x{} at {} bpp, frame {} bytes\n\
             Template size:     {} bytes\n\
             Template slots:    {} ({} currently loaded)\n\
             Template version:  {}\n\
             Current fp mode:   {}\n\
             Encryption status: {:#b}\n\
             Max request size:  {} bytes usable\n\
             Max response size: {} bytes usable\n\
             Template chunking: {} chunks of {} bytes\n",
            info.vendor_id,
            vendor,
            info.product_id,
            info.model_id,
            info.version,
            info.width,
            info.height,
            info.bpp,
            info.frame_size,
            info.template_size,
            info.template_max,
            info.template_valid,
            info.template_version,
            FpMode::display(mode),
            encryption.status,
            input,
            output,
            (info.template_size as usize).div_ceil(chunk),
            chunk,
        ))
    }

    fn match_templates<'a>(
        &'a mut self,
        templates: &'a [Vec<u8>],
        timeout: Duration,
    ) -> BoxFuture<'a, Result<MatchOutput, Box<dyn Error>>> {
        Box::pin(async move {
            let hashes = templates
                .iter()
                .map(|template| {
                    let mut hasher = DefaultHasher::default();
                    template.hash(&mut hasher);
                    hasher.finish()
                })
                .collect::<Vec<_>>();
            if self.enroll_open {
                // An enrolment was abandoned. Clear it out, or the upload below
                // fails with Unavailable and keeps failing forever.
                self.reset_enroll()?;
            }
            self.check_if_templates_got_cleared();
            let deadline = Instant::now() + timeout;
            // FIXME: Figure out why the uploading is in a loop
            let fingerprint_event = loop {
                self.ensure_seed_is_set().await;
                if hashes.iter().collect::<HashSet<_>>()
                    != self.loaded_templates_hashes.iter().collect()
                {
                    fp_mode(&mut self.file, FpMode::Reset as u32)
                        .map_err(|_e| format!("Error doing {:?}", FpMode::Reset))?;
                    self.set_context();
                    // Without waiting a bit, the template uploading can fail.
                    // Maybe 10ms is enough, idk what is the smallest amount that works 99% of the time.
                    sleep(Duration::from_millis(10)).await;
                    for template in templates {
                        upload_template(
                            &mut self.file,
                            &self.protocol_info,
                            &self.fp_info,
                            template,
                        )?;
                    }
                    self.loaded_templates_hashes.clone_from(&hashes);
                }
                // Same reasoning as EnrollImage: only arm the sensor when it is
                // not already armed, so a re-poll does not restart the capture.
                let mode = fp_mode(&mut self.file, FpMode::DontChange as u32)
                    .map_err(|_e| "Error reading fp mode".to_owned())?;
                if mode & FpMode::Match as u32 == 0 {
                    fp_mode(&mut self.file, FpMode::Match as u32)
                        .map_err(|_e| format!("Error doing {:?}", FpMode::Match))?;
                }
                let event = wait_event_timeout(
                    &mut self.file,
                    [EcMkbpEventType::Fingerprint, EcMkbpEventType::HostEvent],
                    deadline.saturating_duration_since(Instant::now()),
                )
                .await?;
                let Some(event) = event else {
                    // Nobody touched the sensor. Hand the driver back so an
                    // enrolment can get in; the caller calls again to keep
                    // waiting. The sensor stays armed, so no touch is lost.
                    return Ok(MatchOutput::TimedOut);
                };
                match event {
                    EcMkbpEvent::Fingerprint(fingerprint_event) => {
                        break fingerprint_event;
                    }
                    EcMkbpEvent::HostEvent(host_event) => {
                        match host_event.rust().map_err(|unexpected_host_event| {
                            format!("Unexpected host event: {unexpected_host_event}")
                        })? {
                            HostEventCode::InterfaceReady => {}
                            event => unreachable!("Unexpected host event: {event:?}"),
                        }
                    }
                    event => unreachable!("Unknown event: {event:?}"),
                };
            };
            match fingerprint_event.rust() {
                EcMkbpEventFingerprintRust::Match(data) => {
                    Ok(match data {
                        EcMkbpEventFingerprintMatchResult::Match(data) => {
                            MatchOutput::Match(MatchedOutput {
                                index: {
                                    // The order of templates in the sensor may be different from the order of given templates.
                                    // We need to return the index based on the given templates.
                                    let matched_hash = self.loaded_templates_hashes[data.index];
                                    let input_hash_index = hashes
                                        .iter()
                                        .position(|&hash| hash == matched_hash)
                                        // We unwrap because it's impossible for there to be a match for a template that was not inputted
                                        .unwrap();
                                    input_hash_index
                                },
                                updated_template: match data.update {
                                    Some(Ok(_)) => {
                                        let template = fp_download_template(
                                            &mut self.file,
                                            &self.fp_info,
                                            &self.protocol_info,
                                            data.index,
                                        );
                                        let template: Vec<u8> = template.into();
                                        self.loaded_templates_hashes[data.index] = {
                                            let mut hasher = DefaultHasher::default();
                                            template.hash(&mut hasher);
                                            hasher.finish()
                                        };
                                        Some(template)
                                    }
                                    _ => None,
                                },
                            })
                        }
                        EcMkbpEventFingerprintMatchResult::NoMatch(result) => {
                            MatchOutput::NoMatch(match result {
                                Ok(_) => None,
                                Err(error) => Some(match error {
                                    EcMkbpEventFingerprintNoMatchError::LowQuality => {
                                        NoMatchError::LowQuality
                                    }
                                    _ => NoMatchError::Other,
                                }),
                            })
                        }
                    })
                }
                fp_event => Err(format!("Unexpected fp event: {fp_event:?}").into()),
            }
        })
    }
}
