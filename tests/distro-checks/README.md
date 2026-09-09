Image-time runner: `run.sh <image-root>` executes `templar doctor --root <image-root> --json` and fails on
errors. The same checks run at runtime, so image and installed system cannot drift.
