# remothing virtual display driver (Windows)

An indirect display driver (IddCx, user mode) that adds virtual monitors on
request. The server installs it on first use and drives it from
`crates/platform-windows/src/vdd.rs`; see "Virtual displays" in
`docs/ARCHITECTURE.md`.

## Origin

This is [SudoVDA](https://github.com/SudoMaker/SudoVDA) at commit
`a4b09fa`, itself based on Microsoft's Indirect Display Driver sample
(Windows-driver-samples, `video/IndirectDisplay`). Changed here:

- Hardware id `Root\remothing\vdd`, control interface GUID
  `{c66e256f-0f7e-4d04-b1d5-9136bed1fbf0}`, UMDF service `RemothingVdd`,
  device group `RemothingVddGroup`, binary `remothing-vdd.dll`: it lives next
  to an installed SudoVDA (Apollo) without either touching the other's
  devices.
- Settings are read from `HKLM\SOFTWARE\remothing\vdd` (same values as
  SudoVDA's: `gpuName`, `maxMonitors`, `watchdog`, `sdrBits`, `hdrBits`).
- The control device is open to SYSTEM and administrators only (SudoVDA's
  also lets everyone read and write it).
- Names shown by Windows say remothing; the project file builds x64 Release
  only and leaves signing to the workflow.

The control protocol (`vdd-ioctl.h`) is SudoVDA's: add a display (size,
refresh rate, monitor GUID, name, serial; answers adapter LUID and target
id), remove it by GUID, ping. Without a request for `watchdog` seconds (3 by
default) the driver removes every display it added.

## Licenses

- Microsoft's sample code: Microsoft Public License, `LICENSE-MS-PL.txt`.
- SudoVDA's changes: MIT or CC0 at the user's choice (SudoVDA's README).
- remothing's changes: as the rest of this repository.

## Build and signing

`.github/workflows/windows-vdd.yml` builds the driver with the WDK on a
GitHub Windows runner, signs `remothing-vdd.dll` and the catalog made by
Inf2Cat with remothing's own code signing certificate (repository secrets
`VDD_SIGNING_PFX` and `VDD_SIGNING_PASSWORD`), installs it on the runner and
runs `crates/platform-windows/examples/vdd.rs` against it. Its artifact
`remothing-vdd-package` goes into `package/`, from where the server embeds
it at compile time. `package/remothing-vdd.cer` is the public certificate;
installing the driver adds it to the machine's Trusted Root and Trusted
Publisher stores, which is what makes Windows accept the package without
test signing.
