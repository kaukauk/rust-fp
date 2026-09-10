# rust-fp — ChromeOS FPMCU fork, with XFCE touch-to-unlock

A fork of [ChocolateLoverRaj/rust-fp](https://github.com/ChocolateLoverRaj/rust-fp) that makes the
fingerprint reader on **Chromebooks running Linux** actually usable day to day:

- **Fixes the bug that made enrolled fingerprints stop working after the first reboot.**
  Templates could be enrolled but never re-uploaded to the sensor. See
  [the upload chunking bug](docs/changes-from-upstream.md#1-templates-could-never-be-re-uploaded-the-headline-bug).
- **One touch now counts as one scan.** Enrolment used to jump two steps per press and finish in
  two or three touches on near-duplicate images.
- **A GTK settings panel** that appears in the XFCE Settings Manager as *Fingerprint*.
- **Touch-to-unlock for XFCE** via logind, without putting a fingerprint module in your PAM stack.
- **An Arch package** (`rust-fp-cros`) so everything lands in `/usr/bin` and `/usr/lib/systemd`.

`libfprint`/`fprintd` cannot drive this sensor, and the
[chrultrabook docs](https://docs.chrultrabook.com/docs/installing/known-issues.html) still list
*"No fingerprint reader support"* as a known issue on Linux.
[`chrultrabook/libfprint-cros`](https://github.com/chrultrabook/libfprint-cros) was archived
read-only in August 2026. This is a working alternative.

## Does my machine work with this?

One command settles it:

```sh
ls /dev/cros_fp
```

If that file exists, your Chromebook has a fingerprint MCU and this is worth trying. If it does
not, nothing here applies — the sensor is on the embedded controller, not USB, which is why
`lsusb` never shows it.

See **[docs/hardware-support.md](docs/hardware-support.md)** for the device landscape, the four
FPMCU variants, and how to identify yours.

## Developed and tested on

| | |
|---|---|
| Laptop | Lenovo ThinkPad C13 Yoga Chromebook |
| Board | `Morphius` (`Google_Zork`), AMD Ryzen 5 3500C |
| Firmware | MrChromebox-2503.0 (full UEFI) |
| FPMCU | `bloonchipper` — RW `bloonchipper-v2.0.25973-4e2e543` |
| Sensor | FPC 1025 (`vendor_id` `0x20435046` = `"FPC "`), 160×160, 5156-byte templates, 5 slots |
| OS | Arch Linux, kernel 7.0.10, XFCE |

Everything below is verified on that machine. Other FPMCUs should work — the host command
interface is shared — but nobody has confirmed it yet. Reports welcome.

## Install

Arch users can build the package directly:

```sh
git clone -b xfce-fingerprint-unlock https://github.com/kaukauk/rust-fp
cd rust-fp/packaging
makepkg -si
sudo systemctl enable --now rust-fp-dbus-interface.service
systemctl --user enable --now rust-fp-xfce-unlock.service
```

Then enrol a finger in **Settings → Fingerprint**, or with `rust-fp add <name>`.

For other distributions, other desktops, and what each piece actually does, read
**[docs/install.md](docs/install.md)** — it does not assume Arch or XFCE.

## Documentation

| Document | What's in it |
|---|---|
| [docs/install.md](docs/install.md) | Full installation, any distro, with verification steps at each stage |
| [docs/how-it-works.md](docs/how-it-works.md) | Architecture, the FPMCU protocol, and why PAM is deliberately avoided |
| [docs/hardware-support.md](docs/hardware-support.md) | Which laptops this applies to and how to identify your sensor |
| [docs/changes-from-upstream.md](docs/changes-from-upstream.md) | Every change against upstream, why it was made, and the evidence |
| [docs/troubleshooting.md](docs/troubleshooting.md) | Diagnosing a sensor that has stopped responding |

## Relationship to upstream

Upstream is the real work: the driver, the D-Bus service, the CLI and the PAM module are
ChocolateLoverRaj's. This fork adds the XFCE integration and packaging, and fixes bugs that only
show up once you use the thing every day. The fixes are not XFCE-specific and would be worth
sending upstream — see
[the upstreaming notes](docs/changes-from-upstream.md#what-should-go-upstream).

Upstream's own README, including the NixOS module and the KDE PAM setup, is preserved at
[docs/upstream-README.md](docs/upstream-README.md).

## Licence

Upstream ships no licence file and declares no licence in its manifests, so this fork cannot grant
one either. Treat it as all-rights-reserved by the original author unless and until upstream states
otherwise.
