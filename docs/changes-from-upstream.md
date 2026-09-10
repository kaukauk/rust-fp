# Every change against upstream

Fork point: `2d0b547` on [ChocolateLoverRaj/rust-fp](https://github.com/ChocolateLoverRaj/rust-fp).

Two batches of work:

- **Additions** — the XFCE unlock daemon, the settings panel and the Arch package. New code, no
  upstream files touched.
- **Fixes** — bugs in upstream's driver, D-Bus service and CLI, plus bugs in the panel added in the
  first batch. These are what took the thing from "enrols once, then never works again" to usable.

Every claim below was measured on the machine in the [README](../README.md#developed-and-tested-on),
not inferred.

---

## Part 1 — the fixes

### 1. Templates could never be re-uploaded (the headline bug)

**Symptom.** Enrol a finger and unlocking works. Reboot, resume from suspend, or restart the
daemon, and it never works again. The journal fills with:

```
Match failed: org.freedesktop.DBus.Error.Failed:
  "Error uploading template: Response(Unavailable)"
```

**Why it looked intermittent.** `match_templates` only uploads when the templates it was asked for
differ from the ones already in the sensor. Straight after enrolling, the template *is* already
loaded, so matching works without an upload. Every later attempt needs the upload — and the upload
could never succeed. Hence "it worked once and never again".

**Cause.** In `crosec`'s `fp_upload_template`:

```rust
let max_chunk_size =
    protocol_info.max_ec_output_size() - offset_of!(EcParamsFpTemplateWithoutData, data) - 4;
let number_of_chunks = template.buffer().len()
    .div_ceil(protocol_info.max_ec_input_size());
```

Chunks are **sized** by `max_ec_output_size()` — the EC-to-host response limit — but **counted** by
`max_ec_input_size()`, the host-to-EC request limit. A template travels host-to-EC, so the request
limit governs both.

On most ChromeOS hardware the two are equal and the mistake is invisible. This FPMCU reports:

```
max_request_packet_size:  544   ->  max_ec_input_size()  = 536
max_response_packet_size: 256   ->  max_ec_output_size() = 248
```

so the loop sent **10 chunks of 236 bytes** and set the `FP_TEMPLATE_COMMIT` flag after
**2360 of 5156 bytes**. The MCU then tried to decrypt a template that was less than half there and
answered `EC_RES_UNAVAILABLE`. 22 chunks were needed.

**How it was confirmed.** Reproduced with upstream's own `ectool`, with the rust-fp daemon stopped,
so no code from this project was involved:

```
$ sudo ectool fp-mode reset; sudo ectool fp-set-context aaaa...aa
$ sudo ectool fp-upload-template < template.bin
Error: command failed with status Unavailable
```

Uploading 5156 bytes of random data instead gave `InvalidParam`, not `Unavailable` — different
failure, so the template was structurally fine and the EC was rejecting it later, during
decryption. A `--probe` binary then printed the packet sizes above and the exact chunk arithmetic.

**Fix.** `upload_template` in `rust-fp/src/drivers/cros_fp.rs` replaces the crosec function and
derives both the size and the count from `max_ec_input_size()`. Verified: `template_valid` goes
from `0` to `1`, and `rust-fp match` arms with no error after a daemon restart.

This is an upstream `crosec` bug and is worth fixing there —
[see below](#what-should-go-upstream).

### 2. One press counted as two scans

**Symptom.** Progress jumped 20% → 40% on a single touch, and enrolment finished in two or three
presses instead of five.

**Evidence.** Daemon log — events arrive in *pairs* ~300ms apart, with ~1.2s between actual presses:

```
01:12:14.433  InProgress(20)
01:12:14.731  InProgress(40)     <- +298ms, same press
01:12:15.927  InProgress(60)
01:12:16.233  InProgress(80)     <- +306ms, same press
01:12:16.601  Complete           <- +368ms, same press
```

**Cause.** After an image, the driver immediately re-armed `ENROLL_IMAGE` while the finger was
still resting on the sensor, so the MCU captured it again straight away. The extra images are
near-duplicates of the first, so the resulting template is also poorer than it looks.

**Fix.** After each image, set `ENROLL_SESSION | FINGER_UP` and wait for the lift before returning.
`ENROLL_SESSION` must stay in the mask — `FP_MODE` sets the mode outright rather than or-ing into
it, so dropping the flag would abandon the enrolment.

After the fix, one step per press, ~2.5s apart:

```
01:08:01.998  InProgress(20)
01:08:04.993  InProgress(40)
01:08:07.485  InProgress(60)
01:08:10.102  InProgress(80)
01:08:11.235  Complete(5156 byte template)
```

### 3. Cancelling an enrolment disabled the fingerprint reader entirely

**Symptom.** Cancel the Add Fingerprint dialog and fingerprint unlock stops working until the
daemon is restarted.

**Cause.** `enroll_step` waits for a touch while holding zbus's write lock on the interface object.
Kill the client mid-enrolment and that wait never ends, so *every* method — `MatchTemplates`
included — is stuck behind it. Upstream even predicted this:

```rust
// TODO: Have a timeout or something so a faulty app doesn't disable the FP sensor
// until this program is restarted
```

`self.enrolling_id` was also only cleared on successful completion, so an abandoned enrolment
reserved the sensor permanently.

**Evidence.** Start an enrolment, `kill -9` it, then ask the daemon anything at all:

```
$ timeout 6 rust-fp get-max-templates
(no output)
rc=124        <- timed out; the entire D-Bus interface is wedged
```

**Fix.** Three parts:

- `EnrollStep` and `MatchTemplates` now wait at most 2 seconds and return `TimedOut`, so the lock is
  released regularly and the caller loops.
- A new `CancelEnroll(id)` D-Bus method resets the sensor and clears the session.
- An enrolment that goes 10 seconds without a step is treated as abandoned and reclaimed.

After the fix the same test returns `Max templates: 5`, `rc=0`.

### 4. The progress dialog never closed, and froze the panel

**Symptom.** At 100% the dialog stays up. Clicking Cancel or the window's X freezes the window.

**Cause.** In the panel's enrolment dialog:

```python
raw, _length = stream.read_line_finish(result)
if raw is None:          # never true
    self.finish()
```

PyGObject returns an **empty bytes object** at end of stream, not `None`. So the "finished" branch
never ran, and the code immediately re-armed another read — which completed instantly with `b''`
again. An unbounded busy loop inside the GTK main loop.

**Evidence.** A headless harness under GTK's broadway backend, driving the dialog with a fake CLI.
Old code versus new:

```
OLD: completes normally: HUNG      in 11.8s using 9.74s CPU   (~82% of a core)
NEW: completes normally: returned OK in  2.1s using 0.12s CPU
```

The freeze was never a deadlock — the main loop was saturated, so it could not repaint or handle
a click.

**Fix.** Treat an empty read as end-of-stream, and guard against re-arming. The dialog now resolves
only when the stream is exhausted *and* the process has been reaped via `wait_async`, so it cannot
close before the sensor is released.

### 5. Cancelling killed the CLI outright

The panel called `Gio.Subprocess.force_exit()` — `SIGKILL`. The CLI had no chance to tell the daemon
to drop the enrolment, so the sensor stayed reserved.

Now the panel sends `SIGTERM`, the CLI catches it and calls `CancelEnroll`, and the dialog waits for
the process to exit before closing. `SIGKILL` remains as a fallback after 5 seconds. Verified for
Cancel, for the window's X, and for a CLI that ignores `SIGTERM` (5.8s, then killed).

### 6. Two panics that killed the daemon

Both would take fingerprint unlock down with the service.

- **`FpMode::from_repr` on a combined mode.** `from_repr` only recognises single flags, so
  `EnrollSession | EnrollImage` — exactly what an abandoned enrolment leaves behind — returned
  `None` and hit `panic!("Unknown fp mode: {fp_mode}")`. Now the bits are tested directly.
- **Template count mismatch.** `check_if_templates_got_cleared` panicked whenever the sensor held
  neither the expected number of templates nor zero. It now clears its cache and lets the next match
  re-upload.

### 7. `set_templates` never truncated the file

```rust
OpenOptions::new().write(true).create(true)   // no .truncate(true)
...
file.write(&vec)                              // may write only part of the buffer
```

Writing a shorter set over a longer one left the tail of the old file behind. Observed directly: the
templates file was 15467 bytes for a payload that needs 7728. MessagePack ignored the trailing
garbage, so it never surfaced as an error — but removing a fingerprint wrote a valid record followed
by junk. Now `.truncate(true)` and `write_all` + `flush`.

### 8. The daemon logged at TRACE

`SimpleLogger::new().init()` defaults to `Trace`, so every `async-io` reactor wakeup went to the
journal, burying the useful lines and writing megabytes. Pinned to `Info`. Idle journal volume over
10 seconds went from thousands of lines to **0**.

### 9. The unlock daemon retried a permanent fault forever

On error it slept 2 seconds and retried, unconditionally, for as long as the screen was locked —
2.3-second intervals, indefinitely, which is what the Sep 09 logs show. Now backs off
exponentially to a 32-second ceiling and resets on success.

### 10. Enrolment left the session open

After a successful enrolment the sensor stayed in `ENROLL_SESSION` mode. Now closed with `RESET`
once the template has been downloaded.

---

## Part 2 — the additions

### `xfce-unlock/` — touch-to-unlock daemon

New crate. Watches `org.xfce.ScreenSaver` for `ActiveChanged`, matches against enrolled templates
while the screen is locked, and calls logind `UnlockSession`. Persists refined templates back to
disk. Deliberately arms the sensor only while locked, so enrolment can get it.

Finds the session by asking logind for a session of class `user` and type `x11`/`wayland`, rather
than trusting `XDG_SESSION_ID` — under `systemd --user` that resolves to the *manager* session, not
the seat session the screensaver locks.

### `xfce-settings/` — GTK3 settings panel

New. Appears in the XFCE Settings Manager as *Fingerprint* via an `X-XFCE-SettingsDialog` desktop
entry, embedding through `--socket-id`. Enrol, remove, and a switch for the unlock service.

### `packaging/` — Arch package

New. Builds `rust-fp-cros` from a tarball of the working tree, installing to `/usr/bin`,
`/usr/lib/systemd/{system,user}`, `/usr/lib/security` and `/usr/share/dbus-1/system.d`. Nothing in
`/usr/local`.

---

## What should go upstream

Items 1, 2, 3, 6, 7, 8 and 10 are not XFCE-specific and are worth sending upstream. Item 1 belongs
in [`crosec-rs`](https://github.com/ChocolateLoverRaj/crosec-rs) rather than `rust-fp`:

```diff
--- a/crosec/src/commands/fp_upload_template.rs
+++ b/crosec/src/commands/fp_upload_template.rs
@@
-    let max_chunk_size =
-        protocol_info.max_ec_output_size() - offset_of!(EcParamsFpTemplateWithoutData, data) - 4;
-    let number_of_chunks = template
-        .buffer()
-        .len()
-        .div_ceil(protocol_info.max_ec_input_size());
+    // A template travels host-to-EC, so the request limit governs both the
+    // chunk size and the chunk count. Sizing by max_ec_output_size() while
+    // counting by max_ec_input_size() silently truncates the upload on any
+    // FPMCU whose response limit is smaller than its request limit.
+    let max_chunk_size =
+        protocol_info.max_ec_input_size() - offset_of!(EcParamsFpTemplateWithoutData, data) - 4;
+    let number_of_chunks = template.buffer().len().div_ceil(max_chunk_size);
```

Items 4 and 5 are bugs in this fork's own panel and do not apply upstream.
