The current screen has the right ingredients, but visually it still reads more like a debug/dashboard canvas than an operating-system front page. The main reason is not merely the empty space. It is that almost everything has equal visual weight and is pushed to the perimeter, leaving no obvious answer to:

What is this screen for?

I would make the answer projects and intentional work.

The taskbar owns running things. PCManFM owns general file navigation. Home should become the place that answers “what am I working on, and what can I start?”

The biggest change: put the important thing in the middle

Right now the center is almost completely empty while low-value things occupy prime positions:

Settings in the upper-right
Desktop files beneath it
Session controls upper-left
Notifications lower-right
parked/running windows lower-left
application launcher floating top-center

That arrangement has no visual center of gravity.

I would instead make the screen roughly:

┌──────────────────────────────────────────────────────────────────────────┐
│  TEMPLAR                                      Search / command       ◉  │
├──────────────────────────────────────────────────────────────────────────┤
│                                                                          │
│  CURRENT                                                                 │
│  ┌────────────────────────────────────────────────────────────────────┐  │
│  │  TEMPLAR                                                building ●   │  │
│  │  PDF / OCR tooling                                                │  │
│  │                                                                    │  │
│  │   >_ Terminal      Files      Edit      Build      Git            │  │
│  │                                                                    │  │
│  │  README.md        renderer/        distro/        3 recent        │  │
│  └────────────────────────────────────────────────────────────────────┘  │
│                                                                          │
│  PROJECTS                                                                │
│  ┌────────────────┐ ┌────────────────┐ ┌────────────────┐               │
│  │ Save Your Skin │ │ jpegXL-rs      │ │ Evidence       │               │
│  │ >_  Files Edit │ │ >_ Files Edit  │ │ >_ Files Edit  │               │
│  └────────────────┘ └────────────────┘ └────────────────┘               │
│                                                                          │
│  RECENT                                               SYSTEM             │
│  report.pdf                                           Battery 96%        │
│  handoff.md                                           187 GB free        │
│  screenshot.png                                       Network ●          │
│                                                                          │
└──────────────────────────────────────────────────────────────────────────┘

That would immediately feel substantially more deliberate.

1. Remove the “boxes around everything” look

The screenshot has many thin outlined rectangles:

Files
Session
Settings
Desktop
Notifications
every parked window

This creates a prototype/toolkit appearance.

You don't need a rectangular border to tell the user that every piece of information belongs together.

Use:

spacing;
typography;
background material;
alignment;
subtle tonal differences;

before borders.

For example, instead of:

┌ Settings ──────────┐
│ Battery 96%        │
│ Volume 120%        │
│ Brightness 100%    │
└────────────────────┘

Home could simply show:

SYSTEM
96% battery     Wi-Fi ●     187 GB free

No panel required.

Reserve strong boundaries for genuinely manipulable objects such as project cards.

2. Delete the Desktop widget

I see very little reason for the upper-right Desktop file listing.

It is effectively:

miniature file manager permanently occupying Home.

That's exactly what we just decided Home shouldn't become.

Replace it with either:

Recent

or:

Pinned

Those are semantic concepts useful on a front page.

If the user wants the Desktop directory:

Files → Desktop

PCManFM handles it.

3. Delete the Session panel

The upper-left panel containing basically:

Lock
Log out

doesn't deserve permanent screen real estate.

Put session/system actions behind one small account/power control in the upper-right:

◉

which opens:

Lock
Suspend
Restart
Shut down
Log out

This is familiar and removes another largely empty box.

4. Notifications shouldn't permanently occupy a quadrant

The lower-right Notifications box displaying:

All caught up

is particularly expensive visually for zero information.

When there are no notifications, there should be no notification panel.

When something happens:

                            ┌──────────────────────┐
                            │ Build completed      │
                            │ Save Your Skin · 4m  │
                            └──────────────────────┘

show a transient card.

A small indicator can expose notification history.

The default state of notification UI should be absence.

That's consistent with the overall TemplarOS philosophy: don't run/display something merely to announce that nothing happened.

5. Current work belongs on Home as well as in the task switcher

The 2026-10-03 user direction supersedes this section's earlier recommendation to remove
running windows from Home. Homegarden is an active-work presentation layer, not only a place
to revisit files after work has ended.

Every open or minimized window is represented by a card on the field itself, not in a
dedicated row: cards take free cells around the centre, the most recently used work first,
and gather by folder — a window's Place association, or else the folder its application most
recently worked in according to the Recent feed (never its title). Terminals, file-manager
folders and image viewers with no folder gather together. A card keeps its cells while its
window lives, so a parked window minimizes into its own area and activation moves nothing;
windows that do not fit are counted on one "+N windows" card that opens the window overview.
Existing same-window previews, activate/restore, minimize and close paths are reused rather
than introducing a second window authority. Closing removes the live card; the existing
remembered files, folders and Places remain under their existing service policy.

The task-switcher popup stays the fast way to reach those windows, and Alt-Tab is unchanged.
Saved user objects do not move when the first window opens or the last one closes.
The implemented contract is described in `docs/architecture/homegarden.md`.

6. Make the application launcher less permanently prominent

The current top-center:

Open an application
Files Kazam Firefox kitty gedit ...

looks like a development toolbar.

I'd make application launching primarily a command/search operation.

For example:

             Search files, apps, projects, commands…

Pressing:

Super

or clicking it can expand:

Applications
Projects
Documents
Commands

You could still have 4–6 pinned apps, but don't let an icon row define the Home screen.

Your Projects are much more important.

7. Give the screen a strong alignment system

The current widgets don't strongly relate geometrically to one another.

Create an underlying grid.

For example at 1920×1080:

outer margin:        32–40 px
base unit:            8 px
major gutter:        24 px

content columns:
[ 1 ][ 2 ][ 3 ][ 4 ][ 5 ][ 6 ][ 7 ][ 8 ][ 9 ][10][11][12]

Then widgets can occupy:

Current project   8 columns
System            4 columns

Project cards     3–4 columns each

Recent            6 columns
Storage           3 columns
Something else    3 columns

The user doesn't see the grid, but they feel it.

The current screen feels scattered because items appear placed around available space rather than composing a whole.

8. Let empty space remain—but make it intentional

You do not need to fill the entire screen.

In fact, a lightweight OS front page can benefit from significant negative space.

The problem with the current empty center is:

everything important is somewhere else.

Good empty space instead surrounds important objects.

Compare:

widgets                              widgets



               nothing



widgets                              widgets

with:


             CURRENT PROJECT
          ┌────────────────────┐
          │                    │
          └────────────────────┘

       project   project   project



The second still contains enormous blank areas, but it looks composed.

9. Make Projects the strongest visual objects

This is where the material idea from earlier becomes useful.

I wouldn't apply marble/crystal/stone everywhere.

That would become visually exhausting.

Use material primarily for persistent identity-bearing things.

For example:

Project
Marble / stone

because a project is persistent.

Information surface
Frosted / very subtle translucent surface

because it is derived information.

Commands/buttons

Mostly plain.

State

Color/light modifies the material.

So:

TEMPLAR

might have a distinctive fine gray marble structure.

Save Your Skin

might have another deterministic mineral structure.

But the surrounding interface stays very restrained.

This produces contrast:

quiet OS
+
distinctive projects

rather than:

everything is fancy
10. Current Project should be much larger than other projects

Not every tile should have equal weight.

If Templar is current:

┌─────────────────────────────────────┐
│ TEMPLAR                               │
│                                     │
│ >_ Terminal  Files  Edit            │
│                                     │
│ build running                       │
│ 4 recent files                      │
└─────────────────────────────────────┘

while dormant projects can be:

┌──────────────┐
│ SaveYourSkin │
│ >_ Files Edit│
└──────────────┘

This provides hierarchy automatically.

One reason Windows 8's tile model became noisy was that too many things competed for attention.

TemplarOS can avoid that by making importance affect representation size.

11. Consider a “project field” rather than a rigid dashboard

You don't necessarily need a conventional fixed dashboard grid.

Because your original concept includes throwable/rearrangeable objects, Home could occupy a middle ground.

Something like:

                  TEMPLAR
             ┌─────────────┐
             │             │
             └─────────────┘

    Save Your Skin              JPEG XL
    ┌────────────┐            ┌────────────┐
    │            │            │            │
    └────────────┘            └────────────┘


             Evidence Intake
              ┌────────────┐
              │            │
              └────────────┘

The objects snap to an invisible grid but retain some spatial organization.

The user could organize:

left = codecs
center = TemplarOS
right = apps

without creating formal folders.

I like that considerably more than making Home look like GNOME's app grid.

12. But use physics only during editing

If you retain the “throwable tiles” idea, avoid perpetual animation.

When arranging:

drag
release
↓
short inertial movement
↓
snap

Then the UI becomes completely static.

No idle bouncing.

No continuously simulated layout.

That fits your performance philosophy.

13. Project groups can become visual regions

You could allow an extremely lightweight grouping object:

CODECS
────────────────────────────────
[ jpegXL-rs ] [ jp2lam ] [ jbig2 ]

OS
────────────────────────────────
[ TemplarOS ]

APPLICATIONS
────────────────────────────────
[ Templar ] [ Evidence ]

These aren't filesystem folders.

They are Home-level organizational groups.

This is exactly the kind of thing Home should do instead of file navigation.

14. Use typography much more aggressively

Right now almost all text looks small and roughly equal.

That contributes heavily to the prototype feeling.

Use perhaps four levels only:

28–32 px    current-project title / major Home heading
18–22 px    project title
13–15 px    normal UI
11–12 px    secondary metadata

For example:

TEMPLAR                       ← strong
PDF processing             ← secondary

>_ Terminal   Files   Edit ← actions

Building · 6m              ← small state

The project card then becomes readable before the user parses individual controls.

15. Don't overuse icons

The row at the top currently mixes tiny app icons with tiny letters/labels. It feels visually noisy despite using very little space.

For Home, I would lean more heavily on:

text;
project material;
simple symbolic icons only for universal actions.

For example:

>_  Terminal
▤   Files
<>  Edit

rather than colorful application icons.

A Templar project should look like the project, not like a collage of the applications used to manipulate it.

16. State colors should be scarce and standardized

This is worth implementing before decorating materials.

Something like:

neutral      normal
cool accent  active/current
green        successfully completed, briefly
amber        attention / degraded
red          failure / action required
gray         unavailable/offline/stale

Crucially:

Do not make every idle object colorful.

If state color means something, neutral should dominate the screenshot.

Then a single amber project actually attracts attention.

17. System information should become one quiet edge region

You currently have Settings occupying a permanent panel.

I'd instead reserve perhaps the upper-right or bottom-right for compact ambient status:

96%    Wi-Fi    187 GB    17:42

Clicking one opens its detailed UI.

That might eventually look like:

                                           96%  ◉  187G  17:42

No huge settings card.

Settings itself is an application.

18. The top-left “Home” button is redundant on Home

The screenshot literally has:

Home

in the upper-left of the Home screen.

That can probably disappear.

If Home is a mode invoked by a key/taskbar control, the screen doesn't need to label itself like a website.

If you want branding there, this could instead be:

Templar

or a small glyph.

But even that is optional.

19. I'd consider making Home slightly darker than application space

Your current almost-white canvas amplifies the huge blank region and makes every small border visible.

I would consider a neutral background around:

warm off-white
soft light gray
charcoal/dark

depending on theme.

Not visually busy wallpaper.

A very subtle texture might fit the Templar idea, but it should be practically imperceptible.

For example:

background = plain mineral-paper-like field
projects   = stronger materials

That gives project objects somewhere to live visually.

20. A concrete redesign from the exact screenshot

I would transform what you have approximately like this:

Delete
upper-left Files mini-widget;
Session box;
Desktop listing;
persistent Notifications box;
parked-window vertical stack;
permanent Home button;
permanent application icon strip;
permanent Settings card.

That's most of the current chrome.

Keep, but transform

Files

→ Recent compact semantic widget.

Settings

→ top-right status strip.

Application launcher

→ search/command palette.

Running windows

→ taskbar.

Add

Current Project — large.

Projects — primary tile field.

Pinned/recent content — secondary.

Maybe one storage/system information surface.

That leaves a very small set of first-class concepts:

Projects
Recent
Search
System status

That's enough.

The design principle I'd use

Templar Home should progressively answer four questions, in this order:

1. What am I doing?

Current project

2. What could I work on?

Other projects/groups

3. What did I recently touch?

Recent/pinned documents

4. What's happening with the machine?

Small system/status indicators

Almost everything visible in the current screenshot answers question 4, or is generic OS plumbing.

That's why it feels empty despite containing quite a few widgets.

Flip that emphasis.

A possible first-pass layout

I would implement this before any marble/crystal work:

┌──────────────────────────────────────────────────────────────────────────┐
│               Search apps, projects, files, commands…     96%  17:42   │
│                                                                          │
│  CURRENT                                                                 │
│  ┌────────────────────────────────────────────────────────────────────┐  │
│  │ TEMPLAR                                                               │  │
│  │ >_ Terminal       Files       Edit                                 │  │
│  │                                                                    │  │
│  │ renderer.rs    distro plan    README        build active           │  │
│  └────────────────────────────────────────────────────────────────────┘  │
│                                                                          │
│  PROJECTS                                                                │
│  ┌──────────────────┐ ┌──────────────────┐ ┌──────────────────┐          │
│  │ Save Your Skin   │ │ jpegXL-rs        │ │ Evidence Intake  │          │
│  │ >_  Files  Edit  │ │ >_  Files  Edit  │ │ >_  Files  Edit  │          │
│  └──────────────────┘ └──────────────────┘ └──────────────────┘          │
│                                                                          │
│  RECENT                                                                  │
│  report.pdf        handoff.md        benchmark.csv                       │
│                                                                          │
│                                                     187 GB free · Wi-Fi │
└──────────────────────────────────────────────────────────────────────────┘

Get that hierarchy, spacing and typography right first.

Then layer your distinctive visual language onto the project surfaces.

That sequence matters. The screenshot's primary problem isn't lack of visual effects—it is lack of hierarchy. Once the screen communicates that Projects are primary, Recent is secondary, and system plumbing is peripheral, the material/state language can make it unmistakably TemplarOS instead of trying to use decoration to solve a structural problem.

>This is decent advice, it just assumes a main project and that the OS knows what it is and that it should somehow be there on first boot. I am interested in making those kind of features that can be added to the screen, but first i am trying to get the initial shape and philosophy of the screen right first. i agree on all redundancy counts and that is what i am trying to avoid right now. things i want to rule out: computer performance stats on front page (noisy, not usually relevant), things that could be menus being widgets instead, like settings. its possible that "recent items" could be expanded to a larger feature like detecting when a lot of work in different files has been done in one folder, and then that folder appears on the home screen with shortcuts to the most used components, like terminals and editors etc. a slightly smart screen could be interesting. what it really needs is much closer integration with the file system, not just one disparate part of it. tiled access to various important parts, which create larger areas when more relevant. this was the first idea, not specialized widgets. kind of an organic system where it detects when you opened something and ran something or somethings in the folder, and it leaves that folder on the home screen in another spot, and removes it after a period of disuse. a garden that gets tended. can even call it homegarden or similar. widgets as such are less interesting because they can usually be menus or full apps. weather and travel planning is usually not important on desktop. control and visibility of important files and apps is what is most important on a desktop.

Yes. This is a much stronger philosophy than “dashboard with widgets.”

I would stop thinking of Home as a page containing features and define it as a spatial model of the user’s current working set.

The useful analogy is exactly your garden idea:

Things appear because you use them, become more prominent when activity concentrates around them, remain where you expect them for a while, and gradually recede when neglected.

That gives TemplarOS a distinctive front page without assuming a “main project,” asking the user to configure one, or filling first boot with generic widgets.

The core model: Home is an activity field

There are three kinds of objects:

Roots are stable places the system knows exist: home directory, major mounted data volumes, perhaps Downloads/Documents if actually used. These are the soil. They don't need giant permanent cards.

Growths are folders or working contexts that emerge from activity. If you repeatedly work inside /data/code/Templar, that directory becomes a Home object automatically.

Pinned objects are things the user has explicitly decided should not decay away.

Everything else is temporary.

So on a relatively fresh system, Home could be almost empty:

                    Search

 Files

      Downloads


                                   Data

Not empty because the UI is unfinished; empty because the machine hasn't learned much yet.

Then you start working.

What makes a folder “grow”

The clever part is that this can use information TemplarOS is already in a position to know, without polling.

Suppose you work in:

/data/code/Templar/

Over an hour, Templar observes:

terminal opened with that CWD;
editor opened several files beneath it;
Git process executed there;
40 files changed underneath it;
PCManFM opened there;
perhaps a build launched there.

Those are not six unrelated “recent items.”

They are evidence of one active working context.

Home should cluster them.

Instead of:

Recent
------
renderer.rs
Cargo.toml
README.md
repo.rs
geometry.rs
...

Home grows:

┌───────────────────────────────┐
│ TEMPLAR                          │
│                               │
│ Terminal    Files    Editor   │
│                               │
│ renderer.rs       README.md   │
│ distro/           crates/     │
└───────────────────────────────┘

And you never had to tell TemplarOS:

“Templar is my current project.”

It inferred only the conservative fact:

A lot of recent intentional activity is concentrated in this directory tree.

That's enough.

Don't call this “Recent”

Recency should be a property of the whole Home environment, not a widget.

The screen effectively becomes a visualized working-set cache.

Your mental working set might currently be:

Templar                         large / active

SaveYourSkin                 medium

jpegXL-rs                    medium

Downloads                    small

tax-document.pdf             small/pinned

Six weeks later it might be completely different.

That's much more useful than a static collection of application widgets.

Activity should change representation size

This is where your earlier “small things become larger things” concept becomes useful.

A folder could progress through stages.

Trace

You've interacted with it once:

[ jp2lam ]

Small, perhaps at the edge of Home.

Active place

Several interactions:

┌──────────────────┐
│ jp2lam           │
│ Files   Terminal │
└──────────────────┘
Working context

Sustained mixed activity:

┌──────────────────────────────────┐
│ JP2LAM                           │
│                                  │
│ Terminal   Files   Editor        │
│                                  │
│ src/encoder.rs     README.md     │
│ tests/             benches/      │
└──────────────────────────────────┘

The user didn't resize a widget.

Its relevance justified more space.

That's an interesting design language.

Activity breadth should matter more than raw activity count

Be careful here.

A directory receiving 100,000 writes because cargo build produced artifacts should not become the center of Home.

A better relevance model looks at human-significant interactions.

High weight:

User opens directory
User opens a file
Terminal starts with cwd here
Editor/workspace starts here
User launches something with this folder as context
User explicitly creates/moves/renames something
User repeatedly returns to the folder

Medium:

Source files modified
Git activity
Files selected
Subdirectories explored

Very low or ignored:

compiler output
cache writes
target/
node_modules/
temporary files
index maintenance
thumbnail generation

This is another reason your unified index needs semantic coverage rather than blindly treating every filesystem event equally.

Use roles, not application names

Once a folder becomes a working context, Home can infer useful actions:

Terminal
Files
Editor

But those should remain semantic.

Home says:

Open terminal at /data/code/Templar
Open folder /data/code/Templar
Edit context /data/code/Templar

The Templar launcher resolves those today to:

kitty
PCManFM-Qt
gedit / code editor / whatever

So Home isn't hardcoded around installed programs.

A programming directory might eventually reveal additional actions based on observed use:

Terminal
Files
Editor
Git UI

But the first three are enough.

You can infer contexts without formally declaring Projects

I think this distinction is useful:

Working context

Automatically inferred.

root = /data/code/Templar
confidence = 0.87
activity = high
Project

Explicit user object.

Could have:

name;
persistent actions;
references;
environment;
launch/restore information;
pinning;
project-specific configuration.

A working context can exist without becoming a Project.

Then perhaps:

Templar has remained active for 12 days

and the UI unobtrusively offers:

Pin

or:

Make permanent

But there is never an annoying wizard saying:

“We detected a project! Create project?”

Automatic discovery should work without demanding housekeeping from the user.

The decay model can have literally zero polling

This fits your no-polling philosophy very neatly.

You don't need a timer that runs every minute:

for every home item:
    relevance *= 0.99

Store:

struct ActivityContext {
    score: f32,
    last_event: InstantLikeTimestamp,
}

When Home next needs the value:

effective_score =
    stored_score × decay(time_since_last_event)

So if you don't open Home for three days, absolutely nothing runs.

When you open it:

stored activity + elapsed time
              ↓
         calculate score
              ↓
        arrange current Home

Or if a new relevant filesystem/session event comes in, update only that context.

Decay is mathematical, not a background process.

That's particularly elegant.

Spatial persistence matters

I would not let the Home rearrange itself constantly.

That would make it clever but unusable.

If Templar is currently left-center, it should remain approximately left-center tomorrow.

Activity changes:

size;
emphasis;
perhaps nearby children;

before it changes location.

Think of gardening literally:

Plants grow where they were planted.

A context should only disappear after a fairly long period of disuse, and surrounding items should close the space gradually rather than everything shuffling after every event.

Something like:

activity rises:
    same position
    tile expands

activity falls:
    tile contracts

low activity:
    becomes small marker

long inactivity:
    disappears

explicit pin:
    never disappears

This preserves spatial memory.

The entire Home can therefore be mostly filesystem-derived

That's a major improvement over the screenshot.

Instead of permanent:

Settings
Session
Notifications
Desktop
performance statistics

the visible surface becomes mostly:

things
places
working contexts

That's much closer to what a desktop computer exists to manipulate.

Menus deal with the machine.

Home deals with your stuff.

I think that's the right philosophical division.

First boot should deliberately be sparse

Your objection to my previous “CURRENT PROJECT” layout is exactly right.

Don't fabricate content to make first boot look impressive.

The initial Home might contain only a few useful roots:

                         Search

               Files

      Home                 Data

               Downloads

Potentially even less.

I wouldn't automatically show Documents, Pictures, Music, Videos, Desktop, Templates, Public, etc. just because XDG created them.

That reproduces the conventional Linux folder hierarchy whether the user cares about it or not.

Show a root when:

it is inherently important;
it has meaningful contents;
the user uses it.

Then after two hours of actual work the screen naturally gets richer.

That's a feature, not a failure.

“Files” itself could be a gateway rather than a widget

I could imagine the permanent filesystem affordance being one simple action:

Files

which opens PCManFM or invokes search.

Then dynamic places grow around it.

For example:

                             Search


          Templar                       SaveYourSkin

     ┌─────────────┐              ┌────────────┐
     │             │              │            │
     └─────────────┘              └────────────┘


                     Files


       Downloads                       JPXL

The whole Home becomes a map of what the filesystem currently means to the user.

Folder activity can also cause children to emerge

This gives you the “tiled access to important parts” idea without static widget definitions.

Suppose Templar has enough room because it is active.

Its most important children could appear:

TEMPLAR
─────────────────────────────

renderer/          distro/

README.md          docs/

>_ Terminal        Files        Edit

Those children are chosen from:

directories frequently entered;
files frequently opened;
recently modified human-facing files;
explicit pins.

If activity shifts from renderer/ to distro/, the representation gradually changes.

Again, no special Templar widget was programmed.

It is just the generic active-directory representation operating on indexed activity.

That's powerful because the same UI works for:

programming;
writing a book;
editing photographs;
organizing legal evidence;
working on a research folder.
Apps can grow out of places too

You said:

detecting when you opened something and ran something or somethings in the folder

Yes.

If you repeatedly do:

kitty cwd=/foo
editor root=/foo
PCManFM /foo

the /foo Home object learns useful launch roles.

You can represent them as tiny sprouts around the folder:

             Editor
               │
Terminal ─── [ foo ] ─── Files

Maybe not literally rendered as a radial graph, but conceptually that's the model.

A folder is the persistent object.

Applications are ways of operating on it.

That reverses the usual desktop model, where applications are primary and files are things opened inside them.

For a desktop focused heavily on projects, I think the folder-first model is much better.

This can also be recursive

Imagine:

/data/code
    ├── Templar
    ├── SaveYourSkin
    ├── jp2lam
    └── jpegXL-rs

If all four become active, Home might notice that their common parent has become significant and form an organic group:

CODE
─────────────────────────────────────

[ Templar ]  [ SYS ]  [ jp2lam ]  [ JPXL ]

You don't necessarily need the user to create a “Code” Home group manually.

But this should be conservative.

Common ancestry alone isn't enough.

Repeated activity across siblings under the same parent is strong evidence.

This is where the filesystem and activity model can create layout naturally.

There should still be manual gardening

Automatic systems become irritating when users cannot override them.

Every Home object should allow at least:

Pin here
Keep
Hide
Remove from Home

Possibly:

Grow
Shrink

or just manual resizing.

The automatic system then treats those as constraints.

For example:

Pin here

means:

position = fixed
minimum prominence = pinned
automatic decay = disabled

while the object can still display changing children/state inside itself.

The machine tends the garden.

The user owns it.

You could have three visibility states

This may be enough rather than lots of complicated scores exposed to UI:

Growing

Active enough to gain representation.

Established

Not currently hot, but familiar/recent enough to remain.

Dormant

No longer displayed, but its identity/history remains available if activity resumes.

So something disappearing from Home doesn't mean the OS forgot it.

It just stopped occupying visual space.

If you return to that directory six weeks later, it can regrow in approximately its old position and perhaps with its old material identity.

That would make the system feel surprisingly coherent.

Material identity becomes much more meaningful in this model

This also improves the marble/crystal idea.

I wasn't entirely satisfied with assigning materials according to abstract widget classes.

Instead:

Material can belong to places.

When a folder becomes an established Home object, give it a deterministic subtle material pattern from its persistent filesystem/project identity.

Then:

/data/code/Templar

looks recognizably like Templar whenever it resurfaces.

The material isn't saying “this is a project.”

It's saying:

this is this place.

State color can still communicate:

active;
attention;
stale/offline;
failed operation;

but the base texture establishes identity.

That's much stronger.

I would define Home around four concepts only

Not widgets.

Place

A directory, drive, file, or user-defined filesystem location.

Context

A Place with enough associated activity to expose tools/relevant children.

Action

Something you can do to a Place:

Open
Terminal
Edit
Search within
Group

A spatial/semantic collection of related Places.

Everything else should probably be a menu, application, taskbar element, transient notification, or search result.

That's a very small vocabulary.

And a small vocabulary is exactly what the screen currently needs.

A possible evolved screen

Not assuming any project is current:

┌──────────────────────────────────────────────────────────────────────┐
│                 Search files, places, apps, commands…               │
│                                                                      │
│                                                                      │
│     TEMPLAR                                  SAVE YOUR SKIN              │
│   ╭──────────────────────╮             ╭────────────────────╮        │
│   │ renderer/   distro/  │             │ engine/   docs/    │        │
│   │ README               │             │                    │        │
│   │                      │             │                    │        │
│   │ Terminal Files Edit  │             │ Terminal Files Edit│        │
│   ╰──────────────────────╯             ╰────────────────────╯        │
│                                                                      │
│                                                                      │
│       JPEG XL                       Downloads                         │
│    ╭────────────╮                ╭────────────╮                       │
│    │ Terminal   │                │ 3 new      │                       │
│    │ Files Edit │                │ Files      │                       │
│    ╰────────────╯                ╰────────────╯                       │
│                                                                      │
│                          Files                                       │
│                                                                      │
└──────────────────────────────────────────────────────────────────────┘

The relative sizes are simply today's activity landscape.

Tomorrow there may be three completely different large objects.

What I'd implement first

Before visual polish, I would build a Home Activity Model.

Feed it only existing authoritative events:

Shared file index
    file/directory activity

Launcher/sessiond
    app launch + associated cwd/path

Terminal launch
    cwd

Editor/file opening
    opened paths

PCManFM
    directories opened

Projects
    explicit project identity when one exists

Normalize events into something like:

struct HomeActivity {
    root: FileIdentity,
    kind: ActivityKind,
    weight: f32,
    timestamp: Timestamp,
}

Then maintain persistent context records:

struct HomeContext {
    root: FileIdentity,
    score: f32,
    last_activity: Timestamp,
    position: Option<HomePosition>,
    pinned: bool,
    observed_roles: RoleSet,
}

The important point is that no new observer daemon is necessary.

Home consumes the event/state infrastructure you're already building.

Then implement only three representation sizes:

trace
tile
expanded

and let activity thresholds choose among them.

That alone is enough to determine whether the philosophy works.

I think “Homegarden” is actually a useful internal name

Maybe not necessarily final branding, but it captures the design better than “dashboard.”

A dashboard says:

Here are instruments reporting on the machine.

A desktop says:

Here are arbitrary icons sitting on a surface.

A launcher says:

Here are applications you could run.

Homegarden says:

Here are the places you've been tending.

That's much closer to what you're describing.

The strongest version of this idea is that there is almost nothing permanently placed on Home except navigation/search. The rest of the surface is a continuously but slowly evolving spatial representation of the files, folders, and working contexts that have actually mattered recently.

That gives you the filesystem integration you're looking for while also solving the current screen's biggest problem: instead of deciding which generic widgets deserve to fill the empty space, the user's own activity earns the space.
