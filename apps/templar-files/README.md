# templar-files

`templar-files` is the standalone TemplarOS file manager. It is intentionally a thin Pixelkit client:
directory ordering and metadata come from `Files.Browse1`, bounded previews come from
`Files.Previews1`, capacity and reserve data come from `Files.Volumes1`, mutations run as
journalled `Files.Operations1` transactions, and opening a file is resolved by `Associations1`
then launched by `Launcher1`.

The application owns presentation state—selection, navigation history, clipboard intent and
dialogs—but no filesystem or association policy. Selecting a supported image, PDF or video loads
a preview into the right rail. BPG, HEIC/HEIF, PDF first pages, common raster formats and
video thumbnails are decoded by bounded service routes, never on the window thread. Closing the
application does not stop an operation already accepted by `templar-filed`.

The visible **Copy path** action asks `Files.Actions1` for the path exactly as shown and places it
on the Wayland clipboard through `wl-copy`. **Properties** is explicit because folder totals may
take time: for a folder it reports recursive regular-file and subfolder counts plus logical and
allocated byte totals, without following symlinks. Partial totals are labelled if a descendant
cannot be read.

Useful keys: Alt+Left/Right (history), Backspace (up), Ctrl+L (location), Ctrl+H (hidden files),
Ctrl+C/X/V (copy/cut/paste), Ctrl+Shift+C (copy path), Alt+Enter (properties), F2 (rename), Delete
(Trash), Ctrl+Z (undo), F5 (refresh), Enter (open). Every action also has a visible button.

Still-image and document previews use the user's own Rust code: `bpg-rs` handles BPG and
HEIC/HEIF, image-rs handles ordinary rasters, and the Templar CPU renderer paints the first PDF
page. The Templar renderer is compiled into the packaged `templar-preview-codecs` companion, which
keeps its heavy dependency graph and lockfile out of the
main OS workspace. `TEMPLAROS_PREVIEW_CODECS` can select a development build. Video is
the deliberate ffmpeg route: ffprobe supplies the duration and ffmpeg seeks to 3% before taking
one bounded frame. No still image or PDF is routed through ffmpeg. `templar-files` depends on
`wl-clipboard` so Copy path has a real compositor clipboard provider rather than an
application-local string.

For a service-backed text smoke without opening a window:

```sh
templar-files --print "$HOME"
```
