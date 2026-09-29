# templar-preview-codecs

This is the bounded PDF preview companion for `templar-filed`. It links the Templar PDF renderer
directly and emits a private `LPV1` RGBA wire image; it is not a
general command-line converter and is not installed on `PATH`.

The nested workspace is intentional: it keeps the Templar renderer's heavy dependency graph
and its own lockfile out of the main TemplarOS workspace. Build, test, stage, and self-check the
companion with:

```sh
just preview-codecs
```

The staged executable lives at `target/preview-codecs/templar-preview-codecs` and the `templaros-filed`
package installs it beside the service in `/usr/libexec`. `TEMPLAROS_PREVIEW_CODECS` overrides that
path for development. Ordinary rasters and BPG/HEIC do not enter this process; video uses the
separately bounded ffmpeg route in `templar-filed`.
