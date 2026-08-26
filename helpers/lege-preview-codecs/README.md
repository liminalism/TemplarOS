# lege-preview-codecs

This is the bounded JPEG XL and PDF preview companion for `lege-filed`. It links the user's JPXL
decoder and Lege PDF renderer directly and emits a private `LPV1` RGBA wire image; it is not a
general command-line converter and is not installed on `PATH`.

The nested workspace is intentional. JPXL and the Lege renderer require Rust 1.97, while the main
LegeOS workspace retains Rust 1.93 for the Ubuntu archive. Build, test, stage, and self-check the
companion with:

```sh
just preview-codecs
```

The staged executable lives at `target/preview-codecs/lege-preview-codecs` and the `legeos-filed`
package installs it beside the service in `/usr/libexec`. `LEGEOS_PREVIEW_CODECS` overrides that
path for development. Ordinary rasters and BPG/HEIC do not enter this process; video uses the
separately bounded ffmpeg route in `lege-filed`.
