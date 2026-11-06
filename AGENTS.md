# For AI agents

This repository is a camera driver (Rust, loaded by Python) for DECTRIS SINGLA detectors in
Instamatic-based programs.

## If you are asked to put the driver into a program

1. Read `INTEGRATION.md` in full before changing anything.
2. Work in the program's tree, on a copy or a branch. Install it editable.
3. Do steps 1–9 in order. Skip the optional ones that do not apply, and say which you skipped.
4. Step 10: run `python -m instamatic.camera.super_dectris.selfcheck --json` and fix every
   `FAIL`. Each result names the step to fix. Report each `WARN` to the user, with your reason for
   leaving it. For a configuration folder other than the default, set the `instamatic`
   variable to it before you run anything of the program.
5. Run `python -m pytest tests/super_dectris` and report the counts, skips included.
6. If a detector is free to use, take one movie with `ctrl.get_movie()` and then one
   `ctrl.get_image()` through the program (step 6).
7. Tell the user what you changed, file by file. Then point them to `docs/DESIGN.md` §8 Tier 3
   and `docs/PROTOCOL.md` §15 for their first runs on the detector ("After the integration" in
   `INTEGRATION.md`).

## Cautions

- **The integration comes first.** Finish steps 1–7 above. Leave the checks of `docs/DESIGN.md`
  §8 Tier 3 to the user, who runs them on the detector after the integration.
- **Do not edit the files in `src/instamatic/camera/super_dectris/`.** They are the driver; the
  program changes are all outside that folder.
- **Do not run `--dcu`, or connect a camera, while another program is using the detector.** A
  second stream client takes frames from the first.
- **Never commit the `.pyd` into the program unless the user asks.**
- **The receive queue length (`zmq_rcvhwm`) depends on how the program pairs frames with the stage
  angle.** Read step 5 and ask the user if it is unclear.
- **This driver is not Instamatic's own `simplon` interface.** Leave that one as it is.

## If you are asked to change the driver itself

- Read `docs/DESIGN.md` first: section 1, then the section about what you change.
- Comments cite it as `DESIGN §n`, and `docs/PROTOCOL.md` as `PROTOCOL §n`.
- Build and test with `src/instamatic/camera/super_dectris/rust/build.ps1` (see the README in that
  folder).
