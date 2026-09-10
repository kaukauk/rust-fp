# Local Arch packaging

Builds `rust-fp-cros`: the upstream rust-fp binaries plus the XFCE touch-to-unlock daemon
(`../xfce-unlock`) and settings panel (`../xfce-settings`).

For installation on other distributions, and for what each component does, see
[../docs/install.md](../docs/install.md).

## Build and install

The PKGBUILD takes its source from a tarball of this working tree rather than from git, because
these additions are not upstream. From a clean checkout, `makepkg -si` is enough. After editing the
code, regenerate the tarball first:

```sh
cd "$(git rev-parse --show-toplevel)"
tar --exclude='./target' --exclude='./.git' --exclude='./packaging' \
    --transform 's,^\.,rust-fp-cros-1.0.0,' \
    -czf packaging/rust-fp-cros-1.0.0.tar.gz .
cd packaging
makepkg -f
sudo pacman -U rust-fp-cros-*.pkg.tar.zst
```

Then enable the services:

```sh
sudo systemctl enable --now rust-fp-dbus-interface.service   # owns /dev/cros_fp
systemctl --user enable --now rust-fp-xfce-unlock.service    # touch-to-unlock
```

Enrol a finger with `rust-fp add <name>`, or through **Settings → Fingerprint**.

## Developing without rebuilding the package

`makepkg` unpacks a fresh source tree every time, so it always builds from scratch — slow on the
target machine. While iterating, build normally and drop the binaries in place:

```sh
cargo build --release -p rust-fp-cli -p rust-fp-dbus-interface -p rust-fp-xfce-unlock
sudo install -m755 target/release/rust-fp                {,/usr/bin/}rust-fp
sudo install -m755 target/release/rust-fp-dbus-interface /usr/bin/rust-fp-dbus-interface
sudo install -m755 target/release/rust-fp-xfce-unlock    /usr/bin/rust-fp-xfce-unlock
sudo install -m755 xfce-settings/rust-fp-settings        /usr/bin/rust-fp-settings
sudo systemctl restart rust-fp-dbus-interface
systemctl --user restart rust-fp-xfce-unlock
```

That leaves the files out of step with the package, which `pacman -Qkk rust-fp-cros` will report.
Run a proper `makepkg -f` + `pacman -U` once you are done to put things back on a package footing.

Cross-building on a faster machine works: both are x86-64, and binaries built against an older
glibc run fine on Arch's newer one. Only `makepkg` itself has to run on the Arch box.

## Note on the PAM module

`librust_fp_pam_module.so` is built and installed but deliberately wired into no PAM stack. XFCE has
a single `auth` stack for `xfce4-screensaver` (unlike KDE's separate `kde-fingerprint` service that
upstream targets), so enabling it would block the password path. Unlocking goes through logind
instead — see [../docs/how-it-works.md](../docs/how-it-works.md#why-not-pam).
