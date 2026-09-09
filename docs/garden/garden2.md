Yes. I would keep the outside agent’s **activity model and stability rules**, but change two parts substantially:

1. **The representation system is too card-like and too development-centric.**
2. **All wall-clock “days” should be removed from Homegarden.**

The current plan gets the underlying philosophy right: Home is a spatial working set, activity earns space, positions remain stable, and machine churn does not count as human activity.  But its visual endpoint is still basically the same rectangle at three sizes—name, roles row, children list—which risks producing a screen full of differently sized file-manager cards. 

I think the next revision should introduce **growth forms**.

## 1. A Place should have a growth form, not a widget type

The underlying object remains exactly the same:

> a filesystem Place with activity, children, actions and persistent identity.

But its presentation adapts to **how the user is using that place**.

Not:

```text
Folder → Folder widget
Project → Project widget
Media → Media widget
```

Instead:

```text
                   PLACE
                     │
            observed interaction
                     │
       ┌─────────────┼─────────────┐
       ↓             ↓             ↓
   Branch form    Shelf form    Stack form
   hierarchy      sequential     documents
   work           media          reading/work
```

This distinction matters. You don't need a new subsystem for every interest.

The same Homegarden object has different presentations.

---

# 2. Tree/branch form is particularly good for development

Your tree idea is better than the current two-column children list.

If activity is concentrated in:

```text
Templar/
├── crates/
│   ├── templar-file-index/
│   ├── templar-window/
│   └── templar-field/
├── docs/
│   └── planning/
└── installer/
```

and those directories have actually been used, Home should reflect that **real containment hierarchy**:

```text
                    TEMPLAR
                     │
           ┌─────────┼────────┐
           │         │        │
        crates      docs   installer
           │         │
      ┌────┴────┐  planning
      │         │
 file-index   window
```

Not literally with ASCII connector lines, necessarily, but visually as a trunk with a few branches.

This is safe philosophically because it isn't inferring a secret relationship. The relationship:

```text
Templar → crates → templar-window
```

already exists in the filesystem.

The current plan explicitly permits children inside a Folder tile because containment is a real visible relationship rather than inferred proximity. 

So I would **reverse the plan's decision to reduce Expanded folders to a six-item two-column list**. That representation throws away exactly the filesystem structure that could make Home distinctive.

### Branch rules

Only draw a branch when that path has earned it through interaction.

So:

```text
Templar/
├── crates/          frequently entered
├── docs/            frequently entered
├── target/          enormous build churn
├── assets/          untouched
└── packaging/       untouched
```

becomes:

```text
             TEMPLAR
            /    \
        crates   docs
```

Not:

```text
             TEMPLAR
       /   /   |   \    \
 crates docs target assets packaging
```

The tree is a map of **used structure**, not a directory browser.

That is the crucial difference.

---

# 3. Branch thickness/size can encode relevance without extra labels

A heavily used branch can simply occupy more visual weight.

For example:

```text
              TEMPLAR
          _____|_____
         /           \
      crates         docs
        |
    ____|____
   /         \
window     file-index
```

If `crates/templar-window` has dominated the current working session, that leaf could be larger.

You don't need:

```text
templar-window
activity score: 17.4
```

The geometry communicates it.

That gets closer to the **right information at the right time** principle.

---

# 4. Media should have a completely different growth form

A directory used primarily for media consumption shouldn't look like source code.

Suppose you routinely watch things under:

```text
/data/video/Documentaries/
```

Homegarden already knows this is a recurring Place because you interact with it.

Now three new videos arrive.

A good representation would be something like a **shelf/filmstrip**:

```text
DOCUMENTARIES
────────────────────────────────────

┌───────┐ ┌───────┐ ┌───────┐
│ new   │ │ new   │ │ 42:18 │
│ video │ │ video │ │resume  │
└───────┘ └───────┘ └───────┘

                    Open folder
```

The information that matters is not:

* every subfolder;
* total GB;
* modification timestamps.

It is:

> **There are two things here you haven't watched yet, and one thing you stopped halfway through.**

That's valuable desktop information.

And importantly, it appears only because:

1. this is a Place you have previously used for watching media;
2. new relevant files appeared there.

A random movie downloaded into a directory you've never used does **not** suddenly generate a Home object.

---

# 5. “Unwatched” needs an explicit consumption signal

I would not guess this from access time.

If TemplarOS wants to know:

```text
new
started
completed
resume
```

it should record actual media consumption.

For an mpv-based default player, for example:

```text
Launcher
   ↓
opens video
   ↓
MediaStarted(path)

player reaches ~90–95% / EOF
   ↓
MediaCompleted(path)
```

and perhaps:

```text
MediaProgress(path, 2520 / 5400)
```

on shutdown/close—not continuously every second.

This can be obtained through the player's IPC/MPRIS integration rather than polling the file.

Then the Homegarden ledger can know:

```rust
enum ConsumptionState {
    New,
    Started { progress: f32 },
    Completed,
}
```

You don't need a media database that crawls the disk.

The shared file index supplies:

```text
file appeared
```

and the media interaction ledger supplies:

```text
user consumed it
```

That's enough.

---

# 6. Other activity forms could remain very small in number

I would resist making twenty specialized visual forms.

Four or five is enough initially.

### Branch

Best when directory hierarchy itself matters.

Typical signals:

* several child directories repeatedly entered;
* terminal/editor/files roles;
* distributed activity beneath a common ancestor.

Typical use:

* programming;
* complex research folders;
* CAD/design projects;
* any deeply hierarchical work.

### Shelf

Best for sequential consumption.

Signals:

* media player repeatedly opening files;
* new media arriving;
* completion/resume state.

Typical use:

* video;
* audio;
* perhaps ebooks.

### Stack

Best for a collection dominated by documents/files rather than subdirectories.

Could look approximately like layered sheets:

```text
        REPORTS
     ┌────────────┐
    ┌────────────┐
   ┌────────────┐
   │ brief.pdf  │
   │ notes.docx │
   │ evidence   │
   └────────────┘
```

Show:

* recently opened;
* recently edited;
* newly arrived if relevant.

Typical use:

* writing;
* case files;
* reports;
* research.

### Gallery

For visually selected collections:

```text
PHOTOS
┌────┬────┬────┐
│    │    │    │
├────┼────┼────┤
│    │    │    │
└────┴────┴────┘
```

Only if image activity makes that useful.

### Plain Place

Fallback.

Just:

```text
┌──────────────┐
│ Downloads    │
│ 3 new        │
│ Open         │
└──────────────┘
```

No need to force everything into an elaborate metaphor.

---

# 7. Do not classify directories permanently

This is an important design constraint.

`~/Downloads` isn't inherently a media folder.

A folder could be:

* video-heavy this session;
* document-heavy next session;
* generic later.

So store something more like:

```rust
struct ContextFacets {
    hierarchy_work: f32,
    media_consumption: f32,
    document_work: f32,
    visual_browsing: f32,
}
```

The renderer chooses the strongest useful presentation at a tending moment.

But changing facet should be conservative.

A Place shouldn't visibly transform from tree → shelf → stack every time you open one unusual file.

Use hysteresis exactly as you do for size.

---

# 8. Better still: distinguish **shape** from **information facet**

There are really two axes:

### Geometry

How the Place occupies Home:

```text
Trace
Compact
Wide
Tall
Expanded
```

### Presentation

What information it shows:

```text
Generic
Hierarchy
Media
Documents
Gallery
```

This avoids an explosion of representations.

For example:

```text
Hierarchy + Expanded → tree

Hierarchy + Small → root name + 2 branch nubs

Media + Wide → horizontal shelf

Media + Small → thumbnail + "2 new"

Documents + Tall → paper stack

Generic + Compact → plain place
```

That's a much more tractable design system.

---

# 9. Keep collision/layout rectangular initially

I would **not yet make the field layout engine handle arbitrary tree-shaped collision masks**.

That's a rabbit hole.

Keep:

```text
Place occupies rectangular grid cells
```

for placement and collision.

Inside that allocated space, draw the foreground structure irregularly:

```text
allocated rectangle
┌─────────────────────────────┐
│           TEMPLAR              │
│            │                │
│      ┌─────┴──────┐         │
│   crates        docs        │
│     │                       │
│  window                     │
│                             │
└─────────────────────────────┘
```

The visual object can have lots of transparent/empty internal space.

So it *looks* organic while the field solver stays simple.

If this works extremely well, you can later investigate masked/polyomino footprints.

Don't make that part of version one.

---

# 10. The plan's time model should be changed completely

I agree strongly with your session-time rule.

The current document is filled with wall-clock assumptions:

* 14 **days** of breadth history;
* 3-day half-life;
* activity on 2/3 distinct days;
* 24/48-hour demotion dwell;
* 5-day dormancy;
* seven days before “Make this a project”;
* 30 days retaining a missing context.  

The open questions even explicitly frame half-life around three, seven or ten calendar days. 

I would remove **all of that**.

If I shut the laptop down for six months:

> Home should look exactly as established when I boot it six months later.

Nothing happened in TemplarOS world.

That is much more coherent.

---

# 11. Introduce a Garden Clock

Don't store Unix time as the meaningful ageing clock.

Maintain:

```rust
pub struct GardenClock {
    pub session_id: u64,
    pub session_hours: f64,
}
```

Or more robustly:

```rust
pub struct GardenMoment {
    /// Number of user sessions completed/entered.
    pub session: u64,

    /// Accumulated Templar user-session runtime.
    pub active_ms: u64,
}
```

`active_ms` advances only while:

```text
Templar user session exists
AND machine is not suspended
AND session is not locked
```

I would probably count unlocked session runtime even if the user hasn't moved the mouse for five minutes.

You don't need to define “human attention” perfectly.

You just want **computer-world age**, not Earth-calendar age.

---

# 12. It can still have zero polling

This does not require a clock daemon ticking every second.

At:

```text
session unlock/start
```

record:

```text
active_interval_started = CLOCK_MONOTONIC
```

At:

```text
lock
suspend
logout
shutdown
```

accumulate:

```text
garden_active_ms += now_monotonic - interval_started
```

When Home is revealed while the interval is still active:

```text
effective_now =
    stored_active_ms
    + monotonic_now - interval_started
```

No timer.

No periodic updates.

Just arithmetic at events/reveal, exactly like the existing zero-polling philosophy.

---

# 13. Use both **session count** and **session hours**

I wouldn't replace “days” with only hours.

The two communicate different things.

Compare:

### A

One 14-hour marathon session.

### B

Six 90-minute sessions in which you keep returning to the same folder.

B is much stronger evidence that the Place belongs on Home long-term.

So breadth should include:

```text
number of distinct sessions
+
amount of session runtime
+
kind/role breadth
```

For example:

```rust
struct ContextAge {
    last_used_session: u64,
    last_used_garden_ms: u64,

    active_sessions: SmallVec<SessionId>,
    accumulated_attention: f32,
}
```

A promotion might require:

```text
Trace → Established

score >= threshold
AND used during >= 2 sessions
```

and:

```text
Established → Expanded

score >= threshold
AND used during >= 3 sessions
AND >= 2 activity modes
```

That's much better than “three distinct days.”

---

# 14. Pruning should mean “you kept using the machine without returning here”

That's the right semantic meaning.

A Place should shrink because:

> While you've been doing other things in TemplarOS, this place has stopped mattering.

Not:

> Tuesday became Friday.

An example starting point:

```text
Expanded → Tile:
    3 later sessions
    AND 6 garden-hours without activity

Tile → Trace:
    6 later sessions
    AND 15 garden-hours without activity

Trace → Dormant:
    10 later sessions
    AND 30 garden-hours without activity
```

Those are illustrative, not magic numbers.

I like requiring **both** conditions.

That prevents:

* one 30-hour session from erasing everything;
* ten 5-minute boot sessions from erasing everything.

A context recedes only after enough actual OS life has happened.

---

# 15. Don't call this “decay” internally if you can avoid it

Once you adopt session time, I'd actually model **competition for current relevance** more than radioactive decay.

A folder isn't getting less meaningful because a clock advances.

It is becoming relatively less relevant because you spent ten sessions elsewhere.

So:

```text
current relevance =
    accumulated evidence
    × recency in garden sessions/hours
    × breadth
```

That feels closer to what Homegarden is actually representing.

---

# 16. New media should be a conditional information bloom

Your media example suggests a powerful general rule:

> **Content changes may alter the information shown inside an already-earned Place, but should rarely create a Place by themselves.**

That fits the outside plan's existing constitution quite well: content isn't supposed to reposition things, and index changes are deliberately weak signals. 

So:

```text
Documentaries is already Established
+
two new video files arrive
+
they have not been watched
        ↓
Documentaries blooms:
"2 new"
+ thumbnails
```

Good.

But:

```text
random unused folder
+
100 videos copied in
        ↓
```

should produce nothing on Home.

This is a strong rule.

It prevents the Home screen from becoming an inbox for machine activity.

---

# 17. “Right information at the right time” can be formalized

For anything proposed for display, ask three questions:

### Is this Place already relevant?

If no, don't surface content merely because it exists.

### Is the information actionable *now*?

Examples:

Good:

```text
2 unwatched videos in a folder you regularly watch
```

Good:

```text
README and docs/ are the parts of this work tree you've been using
```

Probably bad:

```text
Folder contains 6,482 files
```

Probably bad:

```text
Last modified 14:32
```

### Can the information disappear once acted upon?

A good Home bloom should resolve naturally.

Watch the new video:

```text
2 new → 1 new
```

Watch the second:

```text
media shelf contracts back
```

That's the “garden” metaphor operating at the information level.

---

# 18. Separate **structural branches** and **event blooms**

This may be the visual concept you're looking for.

### Branches

Represent persistent/repeated structure.

```text
TEMPLAR
├── crates
│   ├── window
│   └── field
└── docs
```

They grow slowly from repeated use.

### Blooms

Represent temporarily relevant information.

Examples:

```text
Documentaries
   ● 2 unwatched
```

```text
Downloads
   ● ISO arrived
```

```text
Research
   ● 3 new PDFs
```

Blooms disappear when resolved.

This gives Home two timescales:

```text
branches = habit
blooms   = now
```

That is a very strong visual grammar.

It also tells you what should animate, if anything: maybe a new bloom can subtly appear, while branches should never wiggle around.

---

# 19. Different shapes can therefore carry actual semantics

Not file type.

Not arbitrary decoration.

Something like:

| Form               | Meaning                        |
| ------------------ | ------------------------------ |
| **Tree / branch**  | hierarchical repeated activity |
| **Shelf / ribbon** | sequential consumable items    |
| **Stack / folio**  | a working set of documents     |
| **Gallery**        | visual selection               |
| **Plain stone**    | generic place                  |
| **Bud/bloom**      | new actionable information     |

That's a design language.

You could combine it with the material identity idea:

```text
shape = how this Place is being used
material = which Place this is
color = current state
size = current relevance
```

That separation is exceptionally clean:

> **Shape = mode. Material = identity. Color = state. Size = relevance. Position = memory.**

I would write that sentence into the Homegarden design document.

It gives every visual variable exactly one job.

---

# 20. I would revise the outside plan before implementation

The plan is technically strong. In particular, I would retain:

* activity breadth over raw event count;
* build/cache exclusions; 
* no background scanning;
* no live rearranging;
* position persistence;
* no automatically created Projects;
* user overrides;
* tending at reveal/session boundaries;
* roles rather than hard-coded application names.

But I would change the implementation phases.

### Phase 1 — activity + Garden Clock

Before scoring:

* introduce `GardenMoment`;
* remove wall-clock days from the model;
* track session ID and accumulated session runtime;
* make tests simulate sessions rather than dates.

### Phase 2 — structural context

In addition to root attribution:

* accumulate a small **activity tree** per context;
* not just 12 flat `ChildCandidate`s;
* perhaps depth 2–3;
* bounded nodes, e.g. 24/32;
* branches exist only from meaningful activity.

The current plan stores a flat 12-child list.  That's the main data-model limitation I'd fix now rather than after the UI is built.

### Phase 3 — facets

Accumulate conservative evidence for:

```text
Hierarchy
Media
Documents
Gallery
Generic
```

No ML necessary.

### Phase 4 — visual grammar

Build only:

* Generic Place
* Branch Place
* Media Shelf

Those three are enough to prove whether the concept works beyond programming.

### Phase 5 — information blooms

Add:

* new/unwatched media;
* new items in established Downloads;
* perhaps newly arrived documents in established document contexts.

Only content with a clear lifecycle.

### Phase 6 — polish/material

Then do mineral/glass/material identity.

---

## The screen I would aim for

Not:

```text
[Templar card] [Downloads card] [Videos card] [Documents card]
```

but more like:

```text
                   TEMPLAR
                    │
              ┌─────┴─────┐
           crates         docs
             │
          window


                                              DOCUMENTARIES
                                      ═════════════════════════
                                      [new] [new] [continue]


        RESEARCH
       ╱────────╲
      ╱ notes    ╲
     ╱ paper.pdf  ╲
    ╱ brief.docx   ╲


                     Downloads
                       ● 2 new
```

while underneath, collision and placement can still be ordinary bounded rectangular regions.

That would look much less like a dashboard and much more like an operating environment that **organically reflects what the computer has become useful for**.

The change I feel strongest about is your session-time correction. The current plan's calendar-day scoring is conceptually wrong for Homegarden: if the computer is off, **nothing in the garden should age**. Pruning should measure *TemplarOS life experienced since this Place was last tended*, in sessions and session-hours. That principle is cleaner technically and much closer to what you actually want the screen to mean.
