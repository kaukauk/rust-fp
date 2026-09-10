# Hardware support

## The only test that matters

```sh
ls /dev/cros_fp
```

If that character device exists, your machine has a ChromeOS fingerprint MCU and this project
applies. If it does not, nothing here will help.

Do **not** judge by `lsusb`. The sensor is not a USB device — it hangs off the embedded controller —
so it never appears there. That is the single most common reason people conclude the reader is dead;
see [this chrultrabook thread](https://forum.chrultrabook.com/t/lenovo-thinkpad-c13-yoga-chromebook-morphius-fingerprint-reader/2002)
and [MrChromebox/firmware#638](https://github.com/MrChromebox/firmware/issues/638), both about the
exact laptop this was developed on.

Two prerequisites beyond the device existing:

- **Linux, with the `cros_ec` kernel drivers.** Mainline has them; they are enabled by default on
  Arch, Fedora and Ubuntu. `/dev/cros_ec` should exist too.
- **A firmware that still exposes the FPMCU.** MrChromebox's full UEFI ROM does. Verified on
  MrChromebox-2503.0.

## Identify your sensor

Once the daemon is running:

```sh
rust-fp info
```

On the development machine:

```
Driver:            Chromebook (/dev/cros_fp)
Sensor vendor:     0x20435046 ("FPC ")
Sensor product:    9 model 543 version 1
Image:             160x160 at 8 bpp, frame 26260 bytes
Template size:     5156 bytes
Template slots:    5 (1 currently loaded)
Template version:  4
Current fp mode:   0b0 (Reset)
Encryption status: 0b1
Max request size:  536 bytes usable
Max response size: 248 bytes usable
Template chunking: 10 chunks of 524 bytes
```

**Please paste that into an issue if you try this on other hardware**, working or not. It is the
single most useful thing for extending support, because the device-to-FPMCU mapping is not published
anywhere — it lives in per-model ChromeOS config YAML.

## The four FPMCU variants

From the [ChromeOS EC fingerprint documentation](https://chromium.googlesource.com/chromiumos/platform/ec/+/HEAD/docs/fingerprint/fingerprint.md):

| FPMCU | Microcontroller | Sensor | Template size | Expected to work here |
|---|---|---|---|---|
| `bloonchipper` | STM32F412 | FPC 1025 | ~5 KB | **Verified** — this is what was developed on |
| `dartmonkey` | STM32H743 | FPC 1145 | ~48 KB | Very likely; same host commands, larger templates |
| `helipilot` | NPCX99FP | FPC 1025 | ~5 KB | Likely |
| `buccaneer` | NPCX99FP | Elan 80SG | ~19 KB | Plausible, least certain — different sensor vendor |

Nothing in this code hard-codes a template size or a sensor vendor: `template_size` and
`template_max` are read from the sensor at runtime, and chunking is derived from the host-command
limits. The parts most likely to differ on an untested FPMCU are the enrolment percentage steps
(assumed constant, but *derived* from the first step in the UI rather than assumed to be 20%) and
the exact MKBP error codes.

Historic FPMCU board names `nocturne_fp` and `nami_fp` also appear in the EC tree; `dartmonkey`
descends from `nocturne_fp` and `bloonchipper` from `hatch_fp`.

## Which laptops have one

There is no reliable public list, for a reason worth stating plainly:

> **The same model is sold both with and without the sensor.** Whether *your* machine has one
> depends on the configuration, not the model name.

So `ls /dev/cros_fp` remains the answer. That said, ChromeOS devices commonly shipped with a
fingerprint reader include the Lenovo ThinkPad C13 Yoga, Google Pixelbook and Pixelbook Go, HP
Elite c1030 / Dragonfly Pro Chromebook, Acer Chromebook Spin 713, the Framework Laptop Chromebook
Edition, and more recent Chromebook Plus machines such as the ASUS ExpertBook CX54, ASUS Chromebook
Plus CM34 Flip, Lenovo Chromebook Plus 14 and IdeaPad Flex 5i Chromebook Plus. Treat that as a
starting point for checking, not a compatibility list.

## Why this fork matters more than upstream on some devices

The [template upload bug](changes-from-upstream.md#1-templates-could-never-be-re-uploaded-the-headline-bug)
bites on any FPMCU whose **response** packet limit is smaller than its **request** limit. This
machine reports 544 request / 256 response, and upstream's chunking then truncates every upload —
so a fingerprint enrolled with upstream works until the first restart and never again.

`rust-fp info` tells you whether you are affected: if "Max response size" is smaller than
"Max request size", upstream cannot load your saved templates back, and this fork can. Given these
are small microcontrollers with constrained response buffers, that asymmetry is likely common.

## Desktop environment

The unlock daemon here is XFCE-specific: it watches `org.xfce.ScreenSaver` and unlocks through
logind.

| Desktop | Status |
|---|---|
| XFCE | **Verified** — via the `rust-fp-xfce-unlock` daemon in this fork |
| KDE Plasma | Works upstream via the PAM module (`kde-fingerprint`); the driver fixes here apply |
| GNOME | Not working — [upstream issue #3](https://github.com/ChocolateLoverRaj/rust-fp/issues/3) |
| Others | The daemon is ~200 lines; adapting it means changing which screensaver D-Bus name it watches |

Porting to another desktop is mostly a matter of finding the equivalent of `ActiveChanged`. If the
desktop honours logind's `Unlock` signal — most do — the rest carries over unchanged. See
[how-it-works.md](how-it-works.md#why-not-pam) for why logind is used instead of PAM.
