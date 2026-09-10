# How it works

This explains the whole path from a finger on the sensor to an unlocked screen, and why it is put
together this way rather than the usual `fprintd` + PAM arrangement.

## The hardware

On a Chromebook the fingerprint sensor is **not a USB device**. It hangs off a dedicated
microcontroller — the FPMCU — which sits on the embedded controller bus. That is why `lsusb` shows
nothing and why people conclude the reader is dead. The kernel's `cros_ec` driver exposes it as a
character device:

```
/dev/cros_fp
```

Everything talks to the sensor by sending *EC host commands* to that file descriptor via
`ioctl(CROS_EC_DEV_IOCCMD)`, and receives asynchronous notifications (finger down, image captured,
enrolment progress, match result) as *MKBP events* read from the same descriptor.

The crucial property, and the source of most of the design here:

> **The FPMCU does one thing at a time.** It is either idle, or waiting for an enrolment image, or
> waiting for a match. Whoever is waiting on it owns it.

The matching itself happens *on the MCU*, not on the host. The host never sees a fingerprint image
— it only ever handles an encrypted template blob. This is also why `libfprint` cannot drive it:
libfprint is built around host-side matching of images from USB scanners.

### Templates

A template is an opaque, encrypted blob whose size is fixed per sensor (`template_size` from the
`FP_INFO` command — 5156 bytes on FPC 1025). The FPMCU encrypts it with a key derived from:

- an **encryption seed** set once per FPMCU boot (`FP_SET_SEED`), and
- a **context**, effectively a user ID (`FP_CONTEXT`).

This project sets the seed to 32 `'a'` bytes and the context to 32 `0xaa` bytes. Those are
arbitrary but must stay *constant*, because a template enrolled under one pair cannot be loaded
under another. Templates are stored on disk at `~/.var/cros-fp-templates` (mode 600) as MessagePack.

The sensor holds at most `template_max` templates at once (5 here). Loading a saved fingerprint
back means uploading the blob in chunks with `FP_TEMPLATE`, flagging the final chunk `COMMIT`, at
which point the MCU decrypts and validates it. Getting that chunking wrong is what made this fork
necessary — see
[changes-from-upstream](changes-from-upstream.md#1-templates-could-never-be-re-uploaded-the-headline-bug).

## The pieces

```
  ┌──────────────────────────┐
  │ rust-fp-settings         │  GTK3 panel, XFCE Settings Manager
  │ (GUI, runs as you)       │  enrol / remove / toggle unlock
  └────────────┬─────────────┘
               │ spawns, reads stdout
  ┌────────────▼─────────────┐
  │ rust-fp (CLI)            │  add / list / remove / match
  └────────────┬─────────────┘
               │ D-Bus (system bus), org.rust_fp.RustFp
  ┌────────────▼─────────────┐        ┌───────────────────────────┐
  │ rust-fp-dbus-interface   │◄───────┤ rust-fp-xfce-unlock       │
  │ (root, owns /dev/cros_fp)│ D-Bus  │ (user service)            │
  └────────────┬─────────────┘        └─────────────┬─────────────┘
               │ ioctl + MKBP events                │ logind UnlockSession
  ┌────────────▼─────────────┐        ┌─────────────▼─────────────┐
  │ FPMCU (/dev/cros_fp)     │        │ systemd-logind            │
  └──────────────────────────┘        └─────────────┬─────────────┘
                                       Unlock signal │
                                      ┌──────────────▼────────────┐
                                      │ xfce4-screensaver         │
                                      └───────────────────────────┘
```

### `rust-fp-dbus-interface` (system service, root)

Opens `/dev/cros_fp` and is the only thing that touches it. Exposes `org.rust_fp.RustFp` on the
system bus with four methods: `GetMaxTemplates`, `EnrollStep`, `CancelEnroll`, `MatchTemplates`.

Because the sensor serialises, so does this service — zbus takes a write lock on the object for the
duration of any `&mut self` method. That makes **bounded waits mandatory**. Both `EnrollStep` and
`MatchTemplates` wait at most 2 seconds and then return `TimedOut`; the caller simply calls again.
Without this, one client waiting on a touch that never comes freezes every other client, including
the unlock daemon. That is exactly what used to happen when you cancelled an enrolment.

An enrolment is owned by a random 32-bit id. If its client stops calling for 10 seconds the daemon
assumes the client died, cancels the enrolment and gives the sensor back.

### `rust-fp` (CLI)

Thin client over D-Bus. The settings panel drives *this*, rather than speaking D-Bus directly,
because the two wire formats in play — postcard for the D-Bus payloads, MessagePack for the
template file — are both awkward from Python, and the CLI already handles them.

`rust-fp add` installs a SIGTERM/SIGINT handler that calls `CancelEnroll` before exiting, so
stopping an enrolment hands the sensor straight back instead of waiting for the 10-second timeout.

### `rust-fp-settings` (GTK3 panel)

A `Gtk.Plug`-capable panel that the XFCE Settings Manager embeds via `--socket-id`, registered
through an `X-XFCE-SettingsDialog` desktop entry. It lists enrolled fingers, enrols and removes
them, and toggles the unlock service with `systemctl --user`.

The enrolment dialog deliberately does **not** use `Gtk.Dialog.run()`. Cancelling has to keep the
window up until `rust-fp` has actually exited and released the sensor, which `run()` gives you no
way to express. It runs its own `GLib.MainLoop` and resolves only when both the output stream is
exhausted *and* the process has been reaped.

### `rust-fp-xfce-unlock` (user service)

Watches `org.xfce.ScreenSaver` for `ActiveChanged`. While the screen is locked it calls
`MatchTemplates` in a loop; on a match it calls logind's `UnlockSession`. `xfce4-screensaver`
honours logind's `Unlock` signal, which is what makes this work at all.

It arms the sensor **only while the screen is locked**. An always-armed matcher would hold the
sensor and block enrolment.

When the MCU refines a template on a good match, the updated blob is written back to disk, so
recognition improves over time.

## Why not PAM

Upstream ships a PAM module and it works well on KDE, which has a dedicated `kde-fingerprint` PAM
service separate from the password stack. XFCE does not. Two specific problems:

1. **`xfce4-screensaver` has a single `auth` stack.** A fingerprint module placed in it sits in the
   password path too. Since matching blocks until a finger arrives, typing your password would
   stall behind the fingerprint wait.

2. **Upstream's abort mechanism doesn't apply.** The PAM module cancels its wait by listening for
   `org.freedesktop.ScreenSaver`. XFCE does not own that name — it owns `org.xfce.ScreenSaver` —
   so the module would never learn that the screen had been unlocked by password.

Going through logind sidesteps both. The password path is untouched, and a fingerprint unlock is
just an `UnlockSession` call that the screensaver already knows how to obey.

`librust_fp_pam_module.so` is still built and installed, but is wired into no PAM stack. If you run
KDE, use it as upstream documents — you do not need the unlock daemon at all.

## The enrolment sequence

Five touches, 20% each. Per touch:

1. Host sets mode `ENROLL_SESSION | ENROLL_IMAGE`, arming a capture.
2. Host waits for an MKBP fingerprint event with a 2-second deadline.
3. The MCU captures, scores the image and reports a percentage — or an error such as `LowQuality`
   or `Immobile`.
4. **Host sets mode `ENROLL_SESSION | FINGER_UP` and waits for the lift.** This step is the fix for
   the double-count: the MCU will happily photograph a finger that never moved, so without it, one
   press produced two captures of nearly the same image.
5. At 100% the host downloads the encrypted template, closes the session with mode `RESET`, and
   the CLI writes the blob to disk.

Note step 4 keeps `ENROLL_SESSION` in the bitmask. `FP_MODE` *sets* the mode rather than or-ing
into it, so dropping that flag would abandon the enrolment and lose its progress.

## The matching sequence

1. If the templates the caller asked for are not the ones currently loaded, reset the sensor, set
   the context, and upload each template in `max_ec_input_size - 12` byte chunks.
2. Set mode `MATCH`.
3. Wait for a fingerprint event with a 2-second deadline; `TimedOut` hands the sensor back without
   disarming it, so no touch is lost.
4. On a match the MCU returns the index of the matching template, and optionally an updated blob.

The index is the sensor's, not the caller's — the driver maps it back through a hash of each
template, because upload order is not guaranteed to be preserved.

## Waiting without burning CPU or losing events

The obvious way to add a deadline to a blocking read is to race it against a timer and drop the
loser. That is wrong here: `async-std` hands file reads to a blocking thread pool, and dropping the
future does not cancel the read that is already in flight — it stays there and eats the next event.

Instead, `wait_event_timeout` arms the MKBP event mask, then blocks in `poll(2)` on a pool thread
with a real timeout, and only calls in to read once `poll` says an event is queued. Giving up is
then free, because nothing was ever half-read. It also costs nothing while idle: measured at
**0.25% CPU** with the sensor armed, versus a polling loop that would wake the core 40 times a
second for as long as the screen is locked.
