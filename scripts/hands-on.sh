#!/usr/bin/env bash
# Walk the acceptance checks that only a person can answer, and write down what they said.
#
# Most of what TemplarOS still owes is not code: it is 21 acceptance checks whose method is `manual`,
# sitting unsatisfied because nobody has sat down and answered them. This walks them in an order
# that lets one sitting clear a batch, and writes a report the ledger can cite.
#
#   scripts/hands-on.sh                 every stage this machine can answer
#   scripts/hands-on.sh --stage nested  just one stage (nested | installed)
#   scripts/hands-on.sh --list          what would be asked, and answer nothing
#
# It never touches your live session. The nested stage runs scripts/nested-session.sh, which uses
# its own Wayland sockets, its own state and the binaries in target/. The installed stage asks you
# to deploy and log in — deploying is yours to run, and this script will not do it for you.
set -uo pipefail

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)
cd "$root"
stage_filter=""; list_only=0
while [ $# -gt 0 ]; do
    case "$1" in
        --stage) stage_filter="${2:?--stage needs nested or installed}"; shift 2 ;;
        --list) list_only=1; shift ;;
        -h|--help) sed -n '2,15p' "$0"; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done

report="docs/testing/hands-on-$(date +%Y-%m-%d).md"
answered() { [ -f "$report" ] && grep -q "^### $1 " "$report"; }

# id | work record | stage | statement | how to exercise it
checks=(
"hands-on-chrome|representation-chrome-and-continuity|nested|In a nested session, small tiles show content with no permanent band, hover and keyboard focus both reveal manipulation controls, and expansion visibly grows from and collapses back to the tile's place.|Look at a small tile: it should show its content and no permanent title band.\nHover it, then tab to it with the keyboard — both should reveal the same controls.\nExpand it. The growth should start at the tile's own rectangle, not from the screen edge or the centre, and collapsing should return there."
"home-is-always-reachable|home-field|nested|A bare Super tap is the primary Home toggle; Ctrl+Alt+H, Super+D and Home's own button reach the same taskbar-free reveal with every client window hidden, and Escape, pressing again, or activating a window restores the exact prior stack.|Open two or three windows and overlap them so the stacking order is obvious.\nTap Super on its own. Every window should vanish and Home should be bare — no taskbar.\nEscape. The exact stack should come back, same order.\nRepeat with Ctrl+Alt+H, with Super+D, and with Home's own button — all four must reach the same reveal.\nReveal once more and dismiss it by clicking a window instead of pressing a key."
"reveal-survives-shell-death|home-field|nested|Killing templar-shell while a reveal is in effect ends the reveal: every window reappears without user action.|Open two windows, then tap Super so the reveal hides them.\nWith the reveal up, run: scripts/nested-session.sh logs   (to find the shell) then kill the nested templar-shell process.\nBoth windows must come back on their own. You should not have to press anything."
"per-window-compositing|hands-on-run-4-fixes|nested|A window behind a translucent Kitty shows through at Kitty's opacity, and a click on any band or grip focuses that window|Open a kitty with background_opacity below 1, and put another window behind it.\nThe window behind should show through at kitty's opacity — and nothing of the Home Field should show through at all.\nClick the band of the window behind, and its corner grip. Each click should focus that window."
"screen-timeout|hands-on-run-4-fixes|nested|Settings > System shows Screen turns off after; the screen turns off after the chosen idle time and back on at the next input|Open Settings > System. There should be a 'Screen turns off after' control.\nSet it to the shortest option and stop touching the input devices.\nThe surface should blank after that interval, and come back on the next key or pointer movement."
"hands-on-confirmation-run2|hands-on-run-2-fixes|installed|At a real seat: a new window opens centred and can be moved by its top band and resized by its corner grip; the tiling menu visibly arranges; tiles drag-move; the Settings widget opens Settings.|Open a new window — it should arrive centred.\nDrag it by the top band; resize it by the corner grip.\nUse the tiling menu and watch the arrangement happen.\nDrag a tile by its body. Open the Settings widget."
"hands-on-confirmation-run3|hands-on-run-3-fixes|installed|At a real seat after logout/login: a terminal's first line is visible under its band; the band's close, park and maximize buttons work; a window resizes from its corner grip and from just outside its edges; in the Files object a click selects and the verbs act on the selection; Copy path pastes into a terminal; a bare Super press closes the expanded Files object.|Open a terminal: its first line must not be hidden under the band.\nUse close, park and maximize on the band.\nResize from the corner grip, then from just outside an edge.\nIn the Files object, click a row — it should select. Run a verb and confirm it acted on that selection.\nUse Copy path, then paste into the terminal.\nPress Super on its own: the expanded Files object should close."
"hands-on-confirmation-run5|hands-on-run-5-fixes|installed|On the installed desktop: two folder windows open at once, a translucent kitty shows no trace of Home through it, the Desktop tray expands and collapses, and the Files rail shows icons rather than the word 'folder' twice|Open two folder windows at the same time — both should stay open.\nWith a translucent kitty in front of Home, look for any trace of the Home Field through it. There must be none.\nExpand and collapse the Desktop tray.\nLook at the Files rail: icons, not the word 'folder' repeated."
"hands-on-groups|window-groups|installed|At the installed desktop: dropping one band on another lands the dropped window beside the target with one outline around both; Alt+Tab stays inside and Super+Tab crosses; a band drag moves both and Shift+drag one; the group parks and returns from Home as one tile; closing one of two dissolves it; Ctrl+Tab still reaches the browser's tabs.|Drop one window's band onto another's. It should land beside it, with one outline around the pair.\nAlt+Tab should cycle inside the group; Super+Tab should cross to other groups.\nDrag the band: both move. Shift+drag: only one.\nPark the group to Home — it should become one tile — then bring it back.\nClose one of the two: the group should dissolve.\nIn a browser, Ctrl+Tab must still reach its own tabs."
"hands-on-at-a-real-desktop|home-widget-foundations|installed|At a real desktop: drag a tile's corner into free space and nothing else moves; drag it into a neighbour and the neighbour narrows in place without jumping; keep dragging and the ghost visibly stops while the pointer runs on; Ctrl+Z restores both sizes. The Session widget's Shut down arms on the first click and fires on the second. Super+Left/Right steps between windows, and still cycles by stacking order with templar-shell killed.|Drag a tile's corner into empty space: nothing else should move.\nDrag it into a neighbour: the neighbour should narrow where it is, not jump.\nKeep dragging past the limit: the ghost must visibly stop while your pointer keeps going.\nCtrl+Z should restore both sizes.\nClick the Session widget's Shut down once — it should arm, not fire — then click again to fire (or press Escape to disarm).\nSuper+Left/Right should step between windows; kill templar-shell and confirm it still cycles by stacking order."
"hostd-starts|hands-on-run-4-fixes|installed|After deploy, templar-hostd.service is active and templar dev deploy activates every component|After the deploy, run: systemctl status templar-hostd.service\nIt should be active. The deploy's own report should show every component activated."
"snap-browsers-connect|hands-on-run-4-fixes|installed|After logout and login, snap Chromium and Firefox open on the Wayland display|Launch snap Chromium and snap Firefox. Both should open on the Wayland display rather than failing or falling back to X."
)

ask() {
    local id="$1" record="$2" statement="$3" howto="$4"
    printf '\n\033[1m### %s\033[0m  (@templaros.work.%s)\n\n' "$id" "$record"
    printf '  %s\n\n' "$statement"
    printf '%b\n' "$howto" | sed 's/^/    /'
    printf '\n  [p]ass  [f]ail  [s]kip  [q]uit > '
    local answer note
    read -r answer </dev/tty
    case "$answer" in
        p|P) answer=pass ;;
        f|F) answer=fail ;;
        q|Q) return 1 ;;
        *)   answer=skip ;;
    esac
    if [ "$answer" = fail ]; then
        printf '  what happened instead? > '
    else
        printf '  note (optional) > '
    fi
    read -r note </dev/tty
    {
        printf '### %s %s\n\n' "$id" "$answer"
        printf -- '- record: `@templaros.work.%s`\n' "$record"
        printf -- '- statement: %s\n' "$statement"
        [ -n "$note" ] && printf -- '- note: %s\n' "$note"
        printf '\n'
    } >> "$report"
    printf '  recorded: %s\n' "$answer"
    return 0
}

stage_intro() {
    case "$1" in
      nested)
        cat <<'EOF'

════ Stage 1 — nested ════════════════════════════════════════════════════
A whole TemplarOS session inside the desktop you are using now. Nothing here
touches your live session: its own sockets, its own state, target/ binaries.

    scripts/nested-session.sh start        (and `logs` when something is odd)

Start it in another terminal, then come back here.
EOF
        ;;
      installed)
        cat <<'EOF'

════ Stage 2 — the installed session ═════════════════════════════════════
These need the TemplarOS packages installed and a login. Deploying is yours to
run — this script will not do it:

    just deb && sudo ./target/release/templar dev deploy
    log out, pick TemplarOS at the greeter, log back in

None of this needs the distro installed anywhere new. It is the Stage-2
"replacement session on Lubuntu" route you have used for every hands-on run
so far, so it can be answered before you decide where the distro goes.
EOF
        ;;
    esac
    printf '\n  ready? [enter to go on, q to stop] > '
    local go; read -r go </dev/tty
    [ "$go" != q ]
}

if [ "$list_only" = 1 ]; then
    for row in "${checks[@]}"; do
        IFS='|' read -r id record stage _ _ <<< "$row"
        [ -z "$stage_filter" ] || [ "$stage" = "$stage_filter" ] || continue
        printf '%-10s %-28s @templaros.work.%s\n' "$stage" "$id" "$record"
    done
    exit 0
fi

mkdir -p docs/testing
if [ ! -f "$report" ]; then
    {
        printf '# Hands-on run — %s\n\n' "$(date +%Y-%m-%d)"
        printf 'Answers to acceptance checks whose method is `manual`, collected by\n'
        printf '`scripts/hands-on.sh`. Hand this back so the results reach the ledger as\n'
        printf 'evidence; a `fail` is worth more than a `skip`, and both are worth more than\n'
        printf 'an unanswered check.\n\n'
        printf -- '- host: %s\n- commit: %s\n\n' "$(uname -sr)" "$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
    } > "$report"
fi

printf 'Writing to %s — answers are appended as you go, so stopping keeps them.\n' "$report"
last_stage=""
for row in "${checks[@]}"; do
    IFS='|' read -r id record stage statement howto <<< "$row"
    [ -z "$stage_filter" ] || [ "$stage" = "$stage_filter" ] || continue
    if answered "$id"; then printf '\n  %s — already answered, skipping\n' "$id"; continue; fi
    if [ "$stage" != "$last_stage" ]; then
        stage_intro "$stage" || break
        last_stage="$stage"
    fi
    ask "$id" "$record" "$statement" "$howto" || break
done

printf '\n─────────────────────────────────────────────────────────────────────────\n'
printf 'pass %s   fail %s   skip %s\n' \
    "$(grep -c ' pass$' "$report" 2>/dev/null || echo 0)" \
    "$(grep -c ' fail$' "$report" 2>/dev/null || echo 0)" \
    "$(grep -c ' skip$' "$report" 2>/dev/null || echo 0)"
printf 'Report: %s\n' "$report"
