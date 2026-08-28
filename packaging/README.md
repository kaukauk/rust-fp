# Local Arch packaging

Builds `rust-fp-cros`: the upstream rust-fp binaries plus the XFCE touch-to-unlock
daemon (`../xfce-unlock`) and settings panel (`../xfce-settings`).

The PKGBUILD takes its source from a tarball of this working tree rather than from
git, because these additions are not upstream. Regenerate it and build:

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

## Note on the PAM module

`librust_fp_pam_module.so` is built and installed but deliberately wired into no
PAM stack. XFCE has a single `auth` stack for `xfce4-screensaver` (unlike KDE's
separate `kde-fingerprint` service that upstream targets), so enabling it would
block the password path. Unlocking goes through logind instead — see
`../xfce-unlock/src/main.rs`.
