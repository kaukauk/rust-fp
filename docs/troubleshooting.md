# Troubleshooting

Work top to bottom — each check assumes the previous one passed.

## Is the daemon alive and the sensor reachable?

```sh
systemctl status rust-fp-dbus-interface
rust-fp info
```

`rust-fp info` printing sensor details means the whole path — D-Bus policy, daemon, `/dev/cros_fp`,
host commands — is working. Most problems below are diagnosed by *how* this fails.

### It hangs, then times out

Something is holding the sensor. The sensor does one thing at a time, so a stuck caller blocks
everything.

With this fork that should resolve itself: waits are capped at 2 seconds and an abandoned enrolment
is reclaimed after 10. If it does not clear within ~15 seconds:

```sh
sudo systemctl restart rust-fp-dbus-interface
```

If restarting is a *routine* necessity, that is a bug — please file it with `rust-fp info` output
and `journalctl -u rust-fp-dbus-interface -n 100`.

### `org.freedesktop.DBus.Error.ServiceUnknown`

The daemon is not running, or its D-Bus policy is missing:

```sh
ls /usr/share/dbus-1/system.d/org.rust_fp.RustFp.conf
sudo systemctl reload dbus
sudo systemctl restart rust-fp-dbus-interface
```

### `No compatible drivers`

`/dev/cros_fp` is absent or unreadable by root:

```sh
ls -l /dev/cros_*
```

Only `/dev/cros_ec` present means your machine has no FPMCU, or your firmware does not expose it.

---

## Enrolment

### Progress jumps two steps per touch

You are running a build without the finger-up fix. Confirm what is actually installed:

```sh
which rust-fp-dbus-interface
rust-fp --version
```

If you installed over a package with hand-built binaries, `pacman -Qkk rust-fp-cros` will say so.

### "Another program is already enrolling"

Another client holds the enrolment. Wait 10 seconds for the daemon to reclaim it, or:

```sh
pkill -TERM -f "rust-fp add"
```

`SIGTERM`, not `SIGKILL` — the CLI catches it and hands the sensor back cleanly.

### Enrolment never progresses

The sensor is armed but seeing nothing. Check for events at all:

```sh
journalctl -u rust-fp-dbus-interface -f
```

A run of `TimedOut` and nothing else means no touch is being detected. Clean the sensor; press
flat and firmly rather than brushing.

`LowQuality` or `Immobile` errors mean the sensor *is* reading you — shift your finger slightly
between touches.

---

## Matching and unlocking

### `Error uploading template: Response(Unavailable)`

The sensor is refusing a saved template. Two causes:

1. **You are running a build without the upload chunking fix.** Compare the two figures in
   `rust-fp info`:

   ```
   Max request size:  536 bytes usable
   Max response size: 248 bytes usable
   ```

   If the response figure is smaller, upstream's chunking truncates every upload. See
   [the bug](changes-from-upstream.md#1-templates-could-never-be-re-uploaded-the-headline-bug).

2. **The template was enrolled under a different encryption seed or context**, which can happen if
   you have mixed builds that used different constants. There is no way to recover such a template
   — re-enrol:

   ```sh
   rust-fp remove <name>
   rust-fp add <name>
   ```

### It matched once and never again

Historically the signature of the same bug: matching only re-uploads once the sensor has been
cleared, so it worked until the first restart or suspend. Verify with:

```sh
sudo systemctl restart rust-fp-dbus-interface
sleep 3 && rust-fp match
```

### The screen does not unlock, but `rust-fp match` works

The sensor is fine; the problem is the unlock path.

```sh
systemctl --user status rust-fp-xfce-unlock
journalctl --user -u rust-fp-xfce-unlock -n 50
```

- **`Matched <name>. Unlocked session <n>.` but the screen stays locked** — your screensaver is
  ignoring logind's `Unlock` signal. `xfce4-screensaver` honours it; `xscreensaver` does not, and
  will not work with this design.
- **Nothing logged when you touch the sensor** — the daemon only arms while the screen is locked.
  Confirm the lock is registering: `dbus-send --session --print-reply --dest=org.xfce.ScreenSaver
  /org/xfce/ScreenSaver org.xfce.ScreenSaver.GetActive`
- **`Matched <name>, but the screen is already unlocked. Ignoring.`** — working as intended; the
  match was pending from before a password unlock.

### No graphical logind session found

The daemon looks for a logind session of class `user` and type `x11` or `wayland`. Check:

```sh
loginctl list-sessions
loginctl show-session <id> -p Class -p Type -p Remote
```

`XDG_SESSION_ID` is deliberately not used — under `systemd --user` it resolves to the *manager*
session, class `manager`, not the seat session the screensaver locks. If your session shows class
`manager` only, the daemon has nothing to unlock; this can happen over remote or nested sessions.

---

## Starting completely fresh

```sh
systemctl --user stop rust-fp-xfce-unlock
sudo systemctl stop rust-fp-dbus-interface
rm -f ~/.var/cros-fp-templates
sudo systemctl start rust-fp-dbus-interface
systemctl --user start rust-fp-xfce-unlock
rust-fp add index
```

---

## Filing a useful bug report

Include all of:

```sh
rust-fp info
uname -r; cat /etc/os-release | grep PRETTY
cat /sys/class/dmi/id/product_name /sys/class/dmi/id/bios_version
journalctl -u rust-fp-dbus-interface -n 100 --no-pager
journalctl --user -u rust-fp-xfce-unlock -n 50 --no-pager
```

`rust-fp info` is the important one — it identifies the FPMCU, the sensor and the host-command
limits, which is exactly what cannot be looked up from a model name.
