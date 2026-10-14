# Lubuntu and TemplarOS LXQt performance comparison

This protocol compares **Lubuntu running LXQt with TemplarOS running LXQt on the
same physical machine**. It is intended to answer which installation uses less
memory, finishes work faster, stays more responsive under load, and uses less
energy. It specifies future measurements; no benchmarks were run to produce
this document, and it contains no performance results.

Using LXQt on both systems makes a useful comparison possible. It does not by
itself establish equal functionality, equal software versions, or equal resource
use. Conclusions must name the workload and configuration in which an advantage
was measured. The native TemplarOS Homegarden session is outside this comparison;
it can be a separately labelled third configuration later.

## Comparisons to make

| Comparison | Configuration | What the result can establish |
|---|---|---|
| Primary practical comparison | Each installation's supported LXQt session, normal services, default kernel, filesystem, swap and power policy; the same workload content and application settings | Which complete installation is better for this machine's daily use |
| Secondary matched software comparison | Match LXQt, Qt, Openbox, X11 or Wayland backend, graphics driver, application builds and packaging where supported; explicitly list everything still different | Whether the practical difference persists after reducing software and configuration differences |
| Optional attribution experiments | Change one supported setting within one OS, such as swap policy or filesystem placement, then repeat the affected cases | Whether that particular setting explains a difference |

Do not combine these result sets. If versions cannot reasonably be matched, keep
the practical comparison and describe the unmatched versions; do not present it
as proof of a distribution's intrinsic advantage. In the practical comparison,
use the same application versions where readily supported and record exceptions.
Freeze both installations and all benchmark dependencies for the entire campaign.

The TemplarOS backport is pinned in [the LXQt manifest](../../distro/lxqt/backport.json).
Record the actual installed versions on both systems at measurement time rather
than assuming that selecting “LXQt” gives the same stack.

For the matched comparison, also align feasible kernel, filesystem, swap and
power-policy controls, or explicitly leave them as experimental differences.
Matching desktop and application versions alone does not isolate an “OS effect.”

### Establish which TemplarOS features are present

The repository's [native session target](../../session/units/user/templaros-session.target)
starts the compositor, shell, session manager, resource governor and file service.
The [governor unit](../../session/units/user/templar-resourced.service) describes
an observe-only default. These files do not establish that the services run or
enforce policy in an LXQt session.

For each OS, inventory the actual session processes, active system and user
services, autostart entries, D-Bus activations and application cgroups, both at
idle and after the workloads activate services. Record whether TemplarOS indexing
and resource control are absent, observing, or enforcing, and whether the tested
applications are actually subject to that control. **Do not start the native
session target to add these features to LXQt:** it requires the native compositor.

Compare equivalent delivered functions: networking, audio, notifications, locking,
power management, portals and file access. If one installation lacks a tested
function, report that gap alongside its resource saving. Evaluate optional
indexing separately when the other installation provides no equivalent search
service. A smaller process list is an observation, not sufficient evidence of
better efficiency.

## Controls and configuration record

Use native boots of the same computer, with dedicated benchmark accounts and
disposable workload directories. Containers, nested desktops and VMs are useful
for developing a harness but cannot supply the primary desktop comparison.
Boots, logins and preparation belong to a later, owner-operated test session;
do not modify the person's live desktop while preparing the protocol.

| Control | Match or record |
|---|---|
| Hardware | CPU, usable RAM, firmware, microcode, storage model and firmware, GPU, peripherals and connected displays |
| Display | Resolution, refresh rate, scale, brightness, monitor count, window sizes and browser viewport; use the same GPU route and hardware decoding |
| Desktop | LXQt component versions, Qt, window manager, display protocol, compositor and effects, theme, wallpaper, panel widgets and screen locking |
| OS | Release, kernel and command line, package manifest, TemplarOS commit/package versions, drivers, security mitigations and background services |
| Storage | Physical device, filesystem and mount options, encryption, compression, free space, snapshot count, discard policy and location of each workload |
| Memory policy | Swap device and size, swappiness, zram/zswap configuration, OOM policy, cgroup limits and resource-governor mode |
| Power and temperature | AC or battery mode, power profile, CPU scaling policy, turbo, fan mode, battery state and starting CPU/SSD temperatures |
| Applications | Exact versions and packaging, acceleration, extensions, plugins, sandbox settings, profile contents, toolchain, build flags and parallelism |
| Background activity | Update, backup, indexing, thumbnail and synchronization state; network connectivity; any unavoidable scheduled work |
| Measurement | Harness/tool versions, sampling intervals, clock source, dataset hashes, random seed and run order |

Keep AC power, display brightness and a common exposed power profile fixed for
the core campaign. Record how each OS implements that profile. A secondary
matched-policy experiment may align additional tunables; default-policy results
remain separately useful. Start paired runs within 3 °C of the same agreed CPU
and SSD temperature targets and give the machine time to cool between heavy
cases. Sustained throttling during a valid workload is part of the result.

Use local web fixtures and media instead of changing Internet pages. Disconnect
external network access consistently during timed cases, retaining an identical
local fixture server. Provision dependencies and finish updates before the
campaign. Do not disable normal services selectively to improve one OS's score;
record any common scheduling adjustment. A scheduled update that interferes with
a synthetic case can justify a documented rerun, but keep the original capture
and count service-induced interference in the daily-use observations.

Reset account data from the same content manifest, preserving each OS's own
compatible settings. Do not share a mutable home directory or build-output
directory between installations. Maintain separate cold, warmed and first-use
profiles. “Cold” here means the first use after a fresh boot and profile reset,
not a claim that firmware or SSD caches are empty. “Warm” means a specified prior
successful run with the relevant cache retained. Never mix the two populations.
Avoid global cache-dropping commands in the primary daily-use cases.

If the installations occupy different disks, report storage results as a
comparison of those installations and devices. For a secondary storage control,
place test data on the same dedicated scratch volume used by one OS at a time.
That isolates some placement differences but no longer measures each OS's native
filesystem policy. Keep those results separate. Match free-space bands and
preconditioning; use seeded, incompressible data for the synthetic storage cases.

## Workload kit to prepare

Publish a small manifest before collecting results, with checksums, exact versions,
launch arguments, expected output and readiness conditions. Keep the fixtures
unchanged throughout the campaign. The following sizes are proposed protocol
choices and can be adjusted **before** measurement to fit the machine.

| Fixture | Fixed contents |
|---|---|
| Browser | 20 local tabs: ten text/document pages, five fixed JavaScript applications and five image-heavy pages; fixed navigation, typing and scroll sequence |
| Documents | One 100-page PDF and one 100-page office document, with predetermined pages to open, search and edit; same viewer/editor and fonts |
| Files | 100,000 files in 1,000 directories: 90,000 seeded 4 KiB files, 9,000 seeded 64 KiB files and 1,000 fixed JPEGs; a separate seeded 8 GiB file |
| Development | A fixed repository commit and lockfile, pinned toolchain and offline dependencies; one documented source edit for the incremental build |
| Media | A checksummed ten-minute 1080p60 video with fixed codec, audio track and player settings; an optional separate 4K case |
| Mixed work | The browser kit, documents, editor and file manager, plus fixed compilation and file-copy jobs; an event trace with timestamps and expected outcomes |

Use a representative build target whose duration and working set are established
in the later pilot. Freeze the target and an explicit job count then. Both OSes
must produce the same valid output. Keep dependency caches equivalent and build
artifacts private to each installation; a clean build removes only that case's
output, not unrelated caches or personal data.

## Measurements to collect

Collect whole-system counters every second and process proportional memory every
ten seconds, with monotonic timestamps and workload phase markers. Record
application trees, including browser renderers and helpers; executable names alone
are insufficient for grouping short-lived or relaunched processes. Take inventory
snapshots before and after each case. Report missing permissions or unavailable
counters, rather than treating missing measurements as zero.

| Metric | Definition and interpretation |
|---|---|
| Available RAM | `MemAvailable` from `/proc/meminfo`, and the proxy `MemTotal − MemAvailable`; report baseline, phase median and sampled peak proxy. This estimates headroom rather than exact resident consumption. |
| Process memory | PSS from `/proc/PID/smaps_rollup`, grouped into desktop, applications, system services and collector; include anonymous/file/shared breakdowns where available. Avoid summing RSS across shared libraries. |
| Cgroup memory | `memory.current`, `memory.stat`, `memory.swap.current` and event-counter deltas for fixed, nonoverlapping workload groups. Include helpers and record the hierarchy. |
| Swap and compression | Logical swapped bytes, swap-in/out rates and compressed-pool physical consumption, with zram/zswap distinguished. Report these alongside RAM and latency. |
| Resource contention | CPU `some` and memory/I/O `some` and `full` PSI stall percentages for each phase, plus brief spikes. |
| Work completed | Valid operations, build artifacts, copied bytes or frames actually delivered; wall-clock duration, throughput and output checks. |
| Response time | Input-to-visible-response or action-to-ready duration; per-run median, p95, and sufficiently sampled p99; count responses exceeding one second. |
| CPU and storage cost | System and workload CPU-seconds, utilization with normalization stated, read/write bytes, I/O latency, faults, frequency and throttling. |
| Energy | Whole-machine watts and joules per completed task; for a fixed-duration workload also record the amount of useful work delivered. |
| Reliability | Crashes, OOM kills, tab discards, task cancellation, incorrect outputs, stalled UI events and recovery time. |

`MemAvailable` estimates memory available for new applications without swapping;
it is more useful than treating all file cache as unavailable. Changes in this
estimate can also reflect kernel differences. See the
[Linux memory interface](https://man7.org/linux/man-pages/man5/proc_meminfo.5.html).
PSS apportions shared mapped pages between processes; it does not account for all
kernel or GPU memory. Record GPU memory separately where supported, without
adding overlapping counters to a supposed total. See the
[kernel process memory documentation](https://docs.kernel.org/filesystems/proc.html).

Cgroup totals include descendants and their charged memory; never add a parent
total to its children, or add a cgroup total to PSS as though they were disjoint.
Use breakdowns for attribution, not as an exact reconciliation of system RAM.
See the [cgroup v2 memory interface](https://docs.kernel.org/admin-guide/cgroup-v2.html#memory).
Likewise, compressed swap's logical contents and physical pool are different
quantities; a system already paging heavily must not win merely because its
resident applications appear smaller.

For PSI, calculate phase stall percentage as
`100 × change in total microseconds / elapsed phase microseconds`, rather than
averaging overlapping rolling averages. `some` means at least some tasks were
stalled, while `full` means all non-idle tasks were simultaneously stalled.
System-level CPU `full` is undefined and must not be used as a score. See the
[kernel PSI documentation](https://docs.kernel.org/accounting/psi.html).

During the later pilot, measure the collector's overhead and reduce sampling if
it materially changes the workload. Keep the final collector configuration
identical. Reserve intrusive tracing and profilers for separate explanatory runs.
Label memory maxima as **sampled peaks**, except where a validated peak counter
covers precisely the intended interval. A terminal screenshot of `free` or `htop`
is not the measurement record.

## Benchmark cases

| ID | Procedure | Main outcome and supporting measurements |
|---|---|---|
| B1 Boot and login | Ten fresh boots per OS. Record power-on to greeter ready, then authentication accepted to a usable LXQt desktop. Confirm panel, file manager and a typed terminal response work. Separate human password-entry time. | Median/p95 startup duration, failed starts, post-login CPU and memory curve |
| B2 Idle desktop | Record the first ten minutes after login. Separately, after initialization completes, record ten minutes with no applications and screen blanking prevented identically. Record elapsed time to reach this state; do not silently discard a service that never settles. | Median system headroom proxy and desktop PSS, background CPU-seconds, I/O and energy |
| B3 Application launch | QTerminal, PCManFM-Qt, browser, PDF viewer and document editor. Measure first use after reboot separately from 20 warm opens per application. Close ordinary applications normally between attempts; retain desktop providers and label their new-window timings separately. Fixed content must render and respond before the timer stops. | Action-to-ready median/p95, per-application memory and faults; no timing of an immediate-return launcher stub |
| B4 Browser work | Run a pinned Speedometer 3.1 copy in the same graphical browser, then replay the 20-tab kit for ten minutes. Fix viewport, extensions and hardware acceleration; verify the intended pages remain loaded. | Browser score, tab interaction latency, browser-tree PSS, CPU, discards and energy |
| B5 File and document work | Open a directory containing 1,000 fixture images, wait for thumbnails, scroll, search and open specified files. Open/search the fixed PDF and edit/save the office document. Separate new-thumbnail and cached-thumbnail profiles. | First usable listing, thumbnail completion and action p95; output correctness, CPU and memory |
| B6 Compilation | Run a clean build with fixed jobs, then the predetermined incremental edit/build. Use an explicit cache state and offline dependencies. Validate outputs. Repeat with background loads only in B9. | Completed-build duration, CPU-seconds, peak sampled RAM/PSS, I/O and energy |
| B7 Storage | On disposable files, measure 1 MiB sequential read/write at queue depth 1; 4 KiB random read/write separately at depths 1 and 32; buffered copy; 4 KiB write plus `fsync` after each write; and create/stat/rename/delete of the file fixture. | MiB/s or operations/s, p95/p99 I/O latency, durable-write latency, CPU, completed bytes and checksums |
| B8 Media | Play the fixed 1080p60 clip for ten minutes in the same player, fullscreen on the same display, with the same verified hardware-decoding path. Run software decoding as a separate case if wanted. | Dropped frames, CPU/GPU cost, PSS and energy at equal playback quality |
| B9 Mixed daily work | Replay the 15-minute schedule below, once unloaded and once with the same compilation and copy jobs. Preserve the same input trace and workload amounts. | Foreground p95 latency, responses over one second, memory headroom and PSI; background completion and energy |
| B10 Memory pressure | Increase a reproducible real application working set in stages while replaying the same foreground trace. Hold each stage for two minutes. Stop at the first failure boundary defined below, then measure recovery. | Largest successful working set, foreground latency, swap traffic, PSI, discarded/killed work and recovery duration |
| B11 Return to idle and soak | After B9/B10 close the workload normally and observe at 5, 15 and 30 minutes. Separately repeat a fixed open/work/close cycle for four hours. | Remaining anonymous PSS, headroom, background CPU and growth per cycle; distinguish retained useful cache from persistent application growth |
| B12 Energy | Measure 30-minute idle and 30-minute repeated mixed-work windows with identical brightness and power conditions. Also integrate energy over B6 and B9 from start until their required work completes. | Whole-machine mean watts, joules/task, achieved work and latency |

Speedometer measures responsiveness of simulated web applications. Its score is
not a general desktop or kernel score; keep its version and browser fixed and
retain the exported JSON. Use a viewport of at least 850 × 650 pixels and the
same dimensions on both systems. See [Speedometer 3.1](https://browserbench.org/Speedometer3.1/).

For B2, define settled initialization as completed setup/index/thumbnail jobs and
five consecutive one-minute windows with average CPU busy below 2% of total CPU
capacity and background disk writes below 1 MiB/s. Record all windows. If this
condition is not reached within 30 minutes, take the ten-minute observation at
minutes 30–40 and label it **unsettled**, rather than calling it steady idle or
excluding the installation. These thresholds are protocol choices to freeze
after the pilot. Assess first-use initialization separately from a warmed account.

For B3, PCManFM-Qt may already be running as the desktop provider. Do not terminate
it to simulate a cold file-manager process: measure opening its window and state
that the provider is resident. Apply the same classification to any persistent
application on the other OS. Record application setup and closure separately
from the timed launch, and verify no unintended ordinary instance remains.

For B1, use external video timing or a validated common harness for physical
readiness. `systemd-analyze` timings and critical chains can explain startup, but
service startup completion is not proof that the desktop is ready for input.
Keep firmware, OS boot, greeter and session intervals distinct. See the
[upstream systemd manual source](https://github.com/systemd/systemd/blob/main/man/systemd-analyze.xml).

For B7, pin fio and its job definitions. Use an 8 GiB prewritten test file, one
job, a fixed seed and 60-second timed phases after 15 seconds of ramp-up, with
explicit direct/buffered mode and I/O engine. For depths above one, use an
asynchronous engine and retain achieved-depth distributions. Sequential and
random read/write phases are separate, not one opaque aggregate. Direct I/O
bypasses the normal buffered path where supported; it does not make every
filesystem or device equivalent. Keep the buffered and durable-write cases to
represent application behavior. Specify verification outside the timed phase
and identical synchronization at boundaries. See [fio's documentation](https://fio.readthedocs.io/en/latest/fio_doc.html).
Use only the dedicated disposable files, never raw devices, and record actual
bytes written so the write-test cost remains visible.

For GUI latency, use externally observed input and screen response or an
instrumented application with a validated presentation endpoint. Shell-command
exit time and renderer-side animation counters do not establish displayed input
latency. Keep measurement resolution explicit; at 60 Hz one display interval is
about 16.7 ms. If reliable screen timing is unavailable, mark that metric
unmeasured and retain observable action-to-ready timings instead.

### Mixed workload schedule

Restore the warmed profile and open the 20-tab browser, documents, editor and
file manager. The unloaded reference uses the same foreground sequence without
background jobs. For the loaded variant:

1. **Minutes 0–3:** replay a fixed event sequence every ten seconds: switch
   windows, type into the editor, scroll a local page and open a predetermined
   document page. Record completion and visible response for each action.
2. **Minute 3:** start the fixed clean build and one copy of the 8 GiB fixture to
   the disposable destination. Continue the foreground sequence through minute
   10. Record both jobs' actual completion; do not repeatedly restart jobs just
   to keep the machine busy.
3. **Minutes 10–15:** continue the foreground trace with no new background work.
   Record recovery and any remaining jobs. If a job extends past minute 15, keep
   measuring its completion and energy separately; report the overrun.

Use an identical seeded edit/rebuild batch fixed during the pilot if a single
build is too brief to sustain meaningful contention. Do not change the batch
per OS. Report overlapping-load duration, because an OS that finishes background
work sooner naturally experiences less contention. A separately labelled
fixed-duration saturation case can test responsiveness under continuously busy
CPU/I/O, but cannot replace this equal-work comparison.

### Memory pressure and success boundary

Extend the browser kit in identical five-tab groups with fixed active work,
then add the same build workload at a documented stage. Record the successful
functional working set in tabs/jobs and its measured memory, not just virtual
address allocation. Tab suspension or discarding counts as reduced delivered
work unless both configurations deliberately use the same policy.

Use these proposed interactive service targets: p95 visible response at most
100 ms, p99 at most 250 ms when adequately sampled, no response exceeding one second,
and no crash, OOM kill, discard, cancelled task or incorrect output. Stop the
staircase at the first stage that violates a target or the predeclared resource
ceiling; report which limit was reached. Where a stage has too few events to
assess p99, report capacity against the p95/stall/failure targets only and label
the p99 requirement unassessed. A capacity advantage requires more
successfully maintained work at comparable responsiveness, rather than simply
more swap allocation. Repeat boundary stages in independent boots.

A synthetic allocator is an optional diagnostic only: touch and revisit pages,
document its pattern, and do not mistake untouched allocations for resident
load. Reduced-RAM tests are also secondary experiments. A whole-machine RAM cap
must be applied identically in dedicated test boots; a workload cgroup limit
constrains that group, not the OS's total footprint. Record usable RAM for every
configuration. Keep personal applications out of these later pressure sessions.

### Energy measurement

Prefer the same calibrated whole-machine power meter with consistent AC/battery
charging state. If using battery discharge, match starting charge, temperature,
brightness and workload duration, and report measured watt-hours over a substantial
fixed charge range rather than the desktop's estimated remaining time. Charging
must not contaminate AC readings. CPU package energy counters are useful
supporting measurements but do not necessarily include the display, GPU, SSD or
other components, so label their coverage. Lower watts accompanied by much
longer task duration can still mean higher joules per task.

## Repetition and analysis

Run a later pilot solely to establish fixture sizes, instrumentation and duration;
exclude it from the confirmatory results. Freeze the manifest, run order,
thresholds, primary metrics and invalid-run rules after the pilot and before
looking for a winner.

For the core cases, use **ten paired blocks**: each block contains one fresh
boot/session of each OS, with the starting OS randomized and balanced across
blocks. Spread blocks across at least three days where practical. Match starting
conditions and use the same case order within a pair; balance case order between
pairs. Give cold-launch cases their own reboot/profile reset when an earlier
case would warm the application. Long soak/energy extensions may start with
three pairs, but label their estimates exploratory until enough independent
repetitions support useful uncertainty bounds.

Warm launches and interaction events within a boot are repeated observations,
not independent OS installations. Calculate per-boot summaries first, then
compare paired boot summaries. Use at least 1,000 interaction events per OS for
each condition or pressure stage, distributed across blocks, before interpreting
p99; fewer events warrant p95 and stall counts instead. For ten boot observations,
publish the individual
startup times and avoid treating an estimated p95 as a precise tail guarantee.

Report each OS's median and interquartile range, absolute differences, and the
paired improvement with a 95% confidence interval obtained by resampling whole
paired blocks. Keep every run, including failures. An external interruption,
fixture mismatch or broken collector can invalidate a run under the frozen
rules; an OS crash or overload is an outcome, not grounds for deletion. Publish
invalidations and any replacements. Do not keep rerunning until the preferred
OS wins. If uncertainty is too wide, report it and plan a new fixed-size campaign.

Freeze the response timeout in the manifest. A timed-out action or failed build
is not a fast completion and must not silently disappear from latency or
throughput summaries. Report successful-run timings together with failure counts
and timeout bounds; a speed claim must disclose any difference in success rate.

For a lower-is-better metric, define TemplarOS improvement as
`100 × (Lubuntu − TemplarOS) / Lubuntu`. For a higher-is-better metric, use
`100 × (TemplarOS − Lubuntu) / Lubuntu`. Calculate paired values consistently;
do not switch formulas after seeing results. When the baseline is zero, use
absolute differences rather than a percentage.

### Decision rules

The following are **proposed practical margins**, not established universal
standards. Agree or amend them before confirmatory measurement.

| Outcome | Practical margin for a meaningful advantage |
|---|---|
| Steady idle or mixed-work memory proxy | At least 10% **and** 128 MiB less `MemTotal − MemAvailable`, without greater swap pressure or reduced delivered work; corroborate with PSS breakdowns |
| Completed work duration or throughput | At least 5% better, with identical successful outputs |
| Foreground p95 latency | At least 10% and one measurement-resolution unit better, with stalls/failures no worse; also report whether the 100 ms target is met |
| Workload capacity | At least one complete additional staircase stage within the same success targets |
| Energy per completed task | At least 5% lower, with task quality and responsiveness maintained |

Prespecify three headline metrics: B2 steady idle memory proxy, B9 loaded
foreground p95 latency, and B6 clean-build duration. The other measurements
describe tradeoffs and suggest follow-up questions. For headline superiority
claims, use a declared multiplicity correction across these three metrics
(for example, Bonferroni 98.33% two-sided intervals); retain ordinary 95% intervals
for descriptive secondary results and label them accordingly.

Call an OS ahead on a metric only when the entire applicable confidence interval
lies beyond the practical margin in its favor. For the memory rule, require the
percentage and absolute difference intervals to clear their respective margins.
Call results practically equivalent only when the entire interval is inside the
agreed equivalence bounds. Otherwise report **inconclusive**, even if the point
estimate favors one OS. A statistically detectable tiny difference is not
automatically useful, and an inconclusive difference does not prove equality.

Report performance and failure rates together. If the sample is too small to
establish reliability equivalence, say so rather than inferring it from zero
observed crashes. Do not collapse memory, responsiveness, throughput and energy
into an unexplained “overall performance” score. One OS can legitimately lead
on idle overhead while the other leads on sustained throughput or loaded latency.

## Execution order and results record

Start with configuration/functionality checks, then a pilot, then B2, B3, B6,
B9 and B10 as the daily-use core. Add B1/B4/B5/B8 for startup, browser, document
and media behavior; use B7/B11/B12 to explain storage, longer-term memory and
energy differences. Define the three headline metrics before executing the core.
Functionality checks include physical-seat LXQt login, lock, logout and return
to the intended session on both OSes; this protocol does not claim those checks
have already passed.

For every run retain the configuration manifest, fixture/harness hashes, boot
identifier, timestamps, raw counters, event trace, output checks, application
and service inventory, logs, temperatures, power state and failure annotations.
Use one directory per campaign, OS, block and case. Record any departures from
the protocol. Store retained evidence outside disposable scratch directories;
large captures can remain outside Git with stable locations and checksums.

The eventual report should contain these tables, populated from captured data:

| Case and configuration | Independent paired blocks | Lubuntu median and IQR | TemplarOS median and IQR | Absolute difference | Paired improvement and interval | Failures or unmet functions | Verdict |
|---|---|---|---|---|---|---|---|
| To be measured | — | — | — | — | — | — | — |

| Workload phase | System memory proxy | Desktop PSS | Application PSS | Service PSS | Swap and compression | Memory/I/O PSI | Completed work and response targets |
|---|---|---|---|---|---|---|---|
| To be measured separately for each OS | — | — | — | — | — | — | — |

Include time plots for post-login initialization, mixed-work latency and memory,
pressure-stage failures, and return to idle. Keep raw points visible alongside
summary charts. List remaining version, hardware and functionality differences
next to conclusions they could affect.

The conclusions should answer specific questions: which OS has lower idle
overhead, which completes the same work sooner, which keeps foreground work
usable during contention, which maintains more work within the RAM budget, and
which uses less energy for that work. Attribute a difference to a particular
TemplarOS feature only after verifying that feature was active and a separate
controlled experiment supports the attribution.
