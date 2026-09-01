# lege-preview-codecs

This is the bounded JPEG XL and PDF preview companion for `lege-filed`. It links the user's JPXL
decoder and Lege PDF renderer directly and emits a private `LPV1` RGBA wire image; it is not a
general command-line converter and is not installed on `PATH`.

The nested workspace is intentional: it keeps JPXL's and the Lege renderer's heavy dependency graph
and its own lockfile out of the main LegeOS workspace. Build, test, stage, and self-check the
companion with:

```sh
just preview-codecs
```

The staged executable lives at `target/preview-codecs/lege-preview-codecs` and the `legeos-filed`
package installs it beside the service in `/usr/libexec`. `LEGEOS_PREVIEW_CODECS` overrides that
path for development. Ordinary rasters and BPG/HEIC do not enter this process; video uses the
separately bounded ffmpeg route in `lege-filed`.
