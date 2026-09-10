# Installation

Two routes. **Arch** gets a real package. **Anything else** installs by hand — still
straightforward, and each stage has a check so you find out immediately if something is wrong.

Before starting, confirm the hardware exists:

```sh
ls /dev/cros_fp
```

No such file, no point continuing — see [hardware-support.md](hardware-support.md).

---

## Route 1 — Arch Linux

```sh
git clone -b xfce-fingerprint-unlock https://github.com/kaukauk/rust-fp
cd rust-fp/packaging
makepkg -si
```

`makepkg` builds a tarball of the working tree and installs `rust-fp-cros`, which places:

| Path | What |
|---|---|
| `/usr/bin/rust-fp` | CLI |
| `/usr/bin/rust-fp-dbus-interface` | system daemon that owns `/dev/cros_fp` |
| `/usr/bin/rust-fp-xfce-unlock` | touch-to-unlock daemon |
| `/usr/bin/rust-fp-settings` | GTK settings panel |
| `/usr/lib/security/librust_fp_pam_module.so` | built, but wired into no PAM stack |
| `/usr/share/dbus-1/system.d/org.rust_fp.RustFp.conf` | D-Bus policy |
| `/usr/lib/systemd/system/rust-fp-dbus-interface.service` | |
| `/usr/lib/systemd/user/rust-fp-xfce-unlock.service` | |
| `/usr/share/applications/rust-fp-settings.desktop` | Settings Manager entry |

Then start the services and skip to [First run](#first-run):

```sh
sudo systemctl enable --now rust-fp-dbus-interface.service
systemctl --user enable --now rust-fp-xfce-unlock.service    # XFCE only
```

To rebuild after changing the code, regenerate the tarball first:

```sh
cd "$(git rev-parse --show-toplevel)"
tar --exclude='./target' --exclude='./.git' --exclude='./packaging' \
    --transform 's,^\.,rust-fp-cros-1.0.0,' \
    -czf packaging/rust-fp-cros-1.0.0.tar.gz .
cd packaging && makepkg -f && sudo pacman -U rust-fp-cros-*.pkg.tar.zst
```

---

## Route 2 — any other distribution

### Dependencies

Rust (1.75+), a C toolchain, and PAM headers. The PAM headers are only needed for the PAM module;
if you are on XFCE you do not need that module and can skip them.

```sh
# Debian / Ubuntu
sudo apt install build-essential libpam0g-dev pkg-config python3-gi gir1.2-gtk-3.0

# Fedora
sudo dnf install gcc pam-devel python3-gobject gtk3

# openSUSE
sudo zypper install gcc pam-devel python3-gobject typelib-1_0-Gtk-3_0
```

Rust itself via [rustup](https://rustup.rs/) if your distro's is older than 1.75.

### Build

```sh
git clone -b xfce-fingerprint-unlock https://github.com/kaukauk/rust-fp
cd rust-fp
cargo build --release
```

No PAM headers? Build only what you need:

```sh
cargo build --release -p rust-fp-cli -p rust-fp-dbus-interface -p rust-fp-xfce-unlock
```

### Install the binaries

```sh
sudo install -Dm755 target/release/rust-fp                  /usr/local/bin/rust-fp
sudo install -Dm755 target/release/rust-fp-dbus-interface   /usr/local/bin/rust-fp-dbus-interface
sudo install -Dm755 target/release/rust-fp-xfce-unlock      /usr/local/bin/rust-fp-xfce-unlock
sudo install -Dm755 xfce-settings/rust-fp-settings          /usr/local/bin/rust-fp-settings
```

### Install the D-Bus policy

Without this the daemon cannot claim its bus name and nothing else can call it.

```sh
sudo install -Dm644 dbus-interface/org.rust_fp.RustFp.conf \
     /usr/share/dbus-1/system.d/org.rust_fp.RustFp.conf
sudo systemctl reload dbus
```

### Create the system service

`/etc/systemd/system/rust-fp-dbus-interface.service`:

```ini
[Unit]
Description=rust-fp D-Bus interface (ChromeOS FPMCU fingerprint access)
After=dbus.service
Requires=dbus.service

[Service]
Type=exec
ExecStart=/usr/local/bin/rust-fp-dbus-interface
Restart=on-failure
RestartSec=2

[Install]
WantedBy=multi-user.target
```

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now rust-fp-dbus-interface.service
```

**Check it worked before going further:**

```sh
rust-fp info
```

That should print your sensor's details. If it hangs or errors, stop here and see
[troubleshooting.md](troubleshooting.md) — nothing downstream can work until this does.

### XFCE only: the unlock daemon

`~/.config/systemd/user/rust-fp-xfce-unlock.service`:

```ini
[Unit]
Description=Fingerprint touch-to-unlock for XFCE (via logind UnlockSession)
PartOf=graphical-session.target
After=graphical-session.target

[Service]
Type=exec
ExecStart=/usr/local/bin/rust-fp-xfce-unlock
Restart=on-failure
RestartSec=5

[Install]
WantedBy=graphical-session.target
```

```sh
systemctl --user daemon-reload
systemctl --user enable --now rust-fp-xfce-unlock.service
systemctl --user status rust-fp-xfce-unlock.service
```

It should log `Watching logind session <n>`. If it says *No graphical logind session found*, see
[troubleshooting.md](troubleshooting.md#no-graphical-logind-session-found).

### XFCE only: the settings panel entry

```sh
sudo install -Dm644 xfce-settings/rust-fp-settings.desktop \
     /usr/share/applications/rust-fp-settings.desktop
sudo update-desktop-database /usr/share/applications
```

*Fingerprint* then appears in the XFCE Settings Manager. It also runs standalone as
`rust-fp-settings`.

### KDE instead of XFCE

Skip the unlock daemon and use upstream's PAM module, which is the better fit there — KDE has a
`kde-fingerprint` PAM service separate from its password stack:

```sh
sudo install -Dm755 target/release/librust_fp_pam_module.so /usr/lib/security/librust_fp_pam_module.so
```

`/etc/pam.d/kde-fingerprint`:

```
auth    sufficient    librust_fp_pam_module.so
account sufficient    librust_fp_pam_module.so
```

Do **not** do this on XFCE. See [how-it-works.md](how-it-works.md#why-not-pam) for why it breaks the
password path there.

---

## First run

Enrol a finger, either in **Settings → Fingerprint → Add Fingerprint…**, or:

```sh
rust-fp add index
```

Expect **five separate touches**, 20% each. Lift your finger fully between them and shift it
slightly so the edges get recorded. If progress jumps two steps per touch you are running a build
without [the finger-up fix](changes-from-upstream.md#2-one-press-counted-as-two-scans).

```sh
rust-fp list
```

### Verify it survives a restart

This is the step that catches the bug that motivated this fork, so do not skip it:

```sh
sudo systemctl restart rust-fp-dbus-interface
sleep 3
rust-fp match          # touch the sensor
```

You want `Ready to match...` and then a match. If instead you get:

```
Error: Failed("\"Error uploading template: Response(Unavailable)\"")
```

your build does not have the upload chunking fix — see
[the bug](changes-from-upstream.md#1-templates-could-never-be-re-uploaded-the-headline-bug).

### Verify unlocking

Lock the screen (`xflock4`) and touch the sensor. It should unlock. Watch it happen with:

```sh
journalctl --user -u rust-fp-xfce-unlock -f
```

Your password keeps working throughout — this never touches the password path.

---

## Uninstalling

```sh
systemctl --user disable --now rust-fp-xfce-unlock.service
sudo systemctl disable --now rust-fp-dbus-interface.service
sudo pacman -R rust-fp-cros                     # Arch
# or remove the files listed above by hand
rm -f ~/.var/cros-fp-templates                  # enrolled fingerprints
```

Templates are stored only in `~/.var/cros-fp-templates`, mode 600. Deleting it removes every
enrolled fingerprint. Nothing is written to the sensor permanently — it is reloaded from that file
each time.
