# TemplarOS gesture-rail source evidence

Inspected archive: `LegeOS-agent-pack-2026-09-17_183700.zip`.
Archive SHA-256: `7f5085432dd145dc641351cf40d8fa6c81e4655b2fc6ff77152c319db33efe80`.
Review date: September 24, 2026. These are literal excerpts from the attached snapshot, not observations from a running TemplarOS installation.
The user says the current app tray has two lines. Preserve that newer design: the archived BarScreen shown below still lays out a single row. Comments in the archive also disagree with newer implementation in places; code takes precedence.

## Pointer-event insertion points

`compositor/templar-comp/src/input.rs`

```text
1044:     fn on_pointer_motion<I: InputBackend>(&mut self, event: &I::PointerMotionEvent) {
1045:         let Some(pointer) = self.seat.get_pointer() else {
1046:             return;
1047:         };
1048:         let current = pointer.current_location();
1049:         let current_under = self.surface_under(current);
1050:         pointer.relative_motion(
1051:             self,
1052:             current_under,
1053:             &RelativeMotionEvent {
1054:                 delta: event.delta(),
1055:                 delta_unaccel: event.delta_unaccel(),
1056:                 utime: event.time(),
1057:             },
1058:         );
1059: 
1060:         let outputs: Vec<Rectangle<i32, Logical>> = self
1061:             .space
1062:             .outputs()
1063:             .filter_map(|output| self.space.output_geometry(output))
1064:             .collect();
1065:         let position = clamp_to_outputs(current + event.delta(), &outputs);
1066:         let under = self.surface_under(position);
1067:         pointer.motion(
1068:             self,
1069:             under,
1070:             &MotionEvent {
1071:                 location: position,
1072:                 serial: SERIAL_COUNTER.next_serial(),
1073:                 time: event.time_msec(),
1074:             },
1075:         );
1076:         pointer.frame(self);
1077:         self.update_perimeter_cursor(to_templar_point(position));
1078:         self.update_dwell(to_templar_point(position));
1079:     }
```

```text
1098:     fn on_pointer_motion_absolute<I: InputBackend>(
1099:         &mut self,
1100:         event: &I::PointerMotionAbsoluteEvent,
1101:         output_name: Option<&str>,
1102:     ) {
1103:         let Some(output) = (match output_name {
1104:             Some(name) => self.space.outputs().find(|o| o.name() == name),
1105:             None => self.space.outputs().next(),
1106:         }) else {
1107:             return;
1108:         };
1109:         let Some(output_geo) = self.space.output_geometry(output) else {
1110:             return;
1111:         };
1112:         // The one place device coordinates become the global logical scene (module doc).
1113:         let pos = event.position_transformed(output_geo.size) + output_geo.loc.to_f64();
1114:         let serial = SERIAL_COUNTER.next_serial();
1115:         let pointer = self.seat.get_pointer();
1116:         let Some(pointer) = pointer else { return };
1117:         let under = self.surface_under(pos);
1118:         pointer.motion(
1119:             self,
1120:             under,
1121:             &MotionEvent {
1122:                 location: pos,
1123:                 serial,
1124:                 time: event.time_msec(),
1125:             },
1126:         );
1127:         pointer.frame(self);
1128:         self.update_perimeter_cursor(to_templar_point(pos));
1129:         self.update_dwell(to_templar_point(pos));
```

## Legacy dwell policy and grab guard

`compositor/templar-comp/src/input.rs`

```text
1336:     /// A dwell at a screen edge: the **top-left** corner is Home ([`DwellIntent::HomeCorner`]),
1337:     /// the bottom-right corner of a window filling the screen is that window's own chrome
1338:     /// ([`DwellIntent::CoveringChrome`]), and the whole bottom edge — bottom-left corner included
1339:     /// — is Peek ([`DwellIntent::BarPeek`]).
1340:     ///
1341:     /// Both have to be the compositor's rule rather than the shell's: whatever is under the
1342:     /// pointer owns it, so under a maximized or fullscreen window the shell never learns the
1343:     /// pointer reached the edge at all. And both need a timer, because a client that is not
1344:     /// animating sends no further motion once the pointer stops — "rested here for a moment" is
1345:     /// exactly the state that separates a deliberate reach from a pointer sweeping past.
1346:     ///
1347:     /// Nothing about the bar surface changes for a peek: it stays mapped, it keeps its exclusive
1348:     /// zone, and so no work area is recomputed and no window reflows on the way in or out. Only
1349:     /// the stacking order does ([`templar_window::WindowService::set_peek_output`]).
1350:     fn update_dwell(&mut self, pos: TemplarPoint) {
1351:         if self.session_lock.locked {
1352:             self.dwell = None;
1353:             return;
1354:         }
1355:         // A peek ends the moment the pointer walks away from it — no dwell on the way out, the
1356:         // same asymmetry the shell's own reveal has. A *reveal* of Home does not end here: it is
1357:         // a deliberate act, and `WindowService::set_home_revealed` (a click, `Super+H`, a window
1358:         // being activated) is what decides when Home goes away again.
1359:         if let Some(peeked) = self.window_service.peek_output().map(str::to_owned)
1360:             && !self.holding_peek(&peeked, pos)
1361:         {
1362:             self.dwell = None;
1363:             if self.window_service.set_peek_output(None) {
1364:                 crate::space_sync::sync_after_change(self);
1365:             }
1366:         }
1367:         // A covering window's own chrome goes the same way, and holds for the same reason: the
1368:         // strip it just showed has to be reachable, so the pointer is allowed anywhere on it.
1369:         if let Some(shown) = self.window_service.covering_chrome()
1370:             && !self.holding_covering_chrome(shown, pos)
1371:         {
1372:             self.dwell = None;
1373:             if self.window_service.set_covering_chrome(None) {
1374:                 crate::space_sync::sync_after_change(self);
1375:             }
1376:         }
1377:         // A held button is a drag, not a request: a window thrown at the corner is not someone
1378:         // asking for Home. `is_grabbed` is true for the whole of a held button (smithay's
1379:         // `DefaultGrab` installs a `ClickGrab` on press) as well as for our own move/resize
1380:         // grabs, so this is both rules at once.
1381:         let intent = if self.pointer_gesture_active() {
1382:             None
1383:         } else {
1384:             self.dwell_intent_at(pos)
1385:         };
1386:         let now = crate::window_bridge::now_monotonic_ns(&self.clock);
1387:         self.dwell = dwell_after_motion(self.dwell.take(), intent, pos, now);
1388:         if self.dwell.is_some() && !self.dwell_timer_pending {
1389:             self.arm_dwell_timer();
1390:         }
1391:     }
1392: 
1393:     /// One timer source per dwell, rescheduled rather than replaced.
1394:     ///
1395:     /// The pointer settling somewhere new restarts the dwell (`dwell_after_motion`), and with a
1396:     /// four-pixel rest slop that happens on most motion events of an approach; inserting a source
1397:     /// per approach — what the first version of this did — would leave a pile of them to fire and
1398:     /// drop. Instead the callback asks `finish_dwell` how much of the dwell is left and waits out
1399:     /// the remainder on the same source.
1400:     fn arm_dwell_timer(&mut self) {
```

```text
1645:     /// Whether a compositor-run pointer gesture (a move or a resize) is in progress.
1646:     fn pointer_gesture_active(&self) -> bool {
1647:         self.seat
1648:             .get_pointer()
1649:             .is_some_and(|pointer| pointer.is_grabbed())
1650:     }
1651: 
1652:     /// One pointer button while the window overview is up. `true` when the mode took the event and
```

## Older single-row bar layout and independent presence

`shell/templar-shell/src/surface/bar_screen.rs`

```text
 495:     fn update_presence(&mut self, ui: &Ui<'_>, area: Rect, data: &BarData<'_>) -> Presence {
 496:         if !data.covered {
 497:             // Nothing covers the strip. Forget the reveal, so the next window that does covers a
 498:             // bar that starts from its hint rather than one already open.
 499:             self.revealed = false;
 500:             self.hint_since = None;
 501:             self.away_since = None;
 502:             return Presence::Open;
 503:         }
 504:         // The compositor's own dwell, arriving as an event. It opens the bar exactly as the
 505:         // hint's dwell does — same flag, same way out — rather than as a state of its own.
 506:         if data.peeked {
 507:             self.revealed = true;
 508:             self.hint_since = None;
 509:             self.away_since = None;
 510:             return Presence::Revealed;
 511:         }
 512:         let inside = ui.input.cursor().is_some_and(|(x, y)| area.contains(x, y));
 513:         if self.revealed {
 514:             if inside {
 515:                 self.away_since = None;
 516:             } else {
 517:                 let away = *self.away_since.get_or_insert(data.now_ms);
 518:                 if data.now_ms.saturating_sub(away) >= HINT_CLOSE_MS {
 519:                     self.collapse();
 520:                     return Presence::Hint;
 521:                 }
 522:             }
 523:             return Presence::Revealed;
 524:         }
 525:         match ui.input.cursor().filter(|(x, y)| area.contains(*x, *y)) {
 526:             Some((x, y)) => {
 527:                 // The dwell restarts wherever the pointer moves to. Resting is the condition, not
 528:                 // merely being low: a pointer crossing the bottom edge on its way somewhere else
 529:                 // never holds still long enough to ask for the bar.
 530:                 let (since, at_x, at_y) = *self.hint_since.get_or_insert((data.now_ms, x, y));
 531:                 if (x - at_x).abs() > HINT_REST_SLOP || (y - at_y).abs() > HINT_REST_SLOP {
 532:                     self.hint_since = Some((data.now_ms, x, y));
 533:                 } else if data.now_ms.saturating_sub(since) >= HINT_OPEN_MS {
 534:                     self.revealed = true;
 535:                     self.hint_since = None;
 536:                     self.away_since = None;
 537:                     return Presence::Revealed;
 538:                 }
 539:             }
 540:             None => self.hint_since = None,
 541:         }
 542:         Presence::Hint
 543:     }
 544: 
 545:     /// The hint: the bar's ground and one `^`, on a surface too thin to hold anything else.
```

```text
 581:     fn layout(ui: &Ui<'_>, area: Rect, data: &BarData<'_>, open_fan: Option<usize>) -> Layout {
 582:         let bar_height = ui.px(BAR_HEIGHT).min(area.h);
 583:         let bar = Rect::new(area.x, area.bottom() - bar_height, area.w, bar_height);
 584:         let chip_h = ui.px(CHIP_H).min(bar.h);
 585:         let chip_y = bar.y + (bar.h - chip_h) / 2;
 586:         let gap = ui.px(CHIP_GAP);
 587: 
 588:         // The chips are centred *inside* the surface, not laid from its left edge. That matters
 589:         // because an open row makes the surface wider than the island, and the surface is centred
 590:         // on the output — so laying the chips from its left edge slid every one of them sideways
 591:         // the moment a row opened, out from under the pointer that opened it. The row would then
 592:         // close, the surface shrink, the chip come back, and the whole thing cycle. Centring the
 593:         // strip means a change of surface width moves nothing the pointer is on.
 594:         //
 595:         // Each chip is still clipped to whatever room is left rather than dropped: a half-width
 596:         // chip shows an icon. The machine's own chips are not here at all — they are the
 597:         // cluster's, on its own surface at the right-hand end of the screen.
 598:         let strip = ui.px(island_width(data.entries.len())).min(bar.w);
 599:         let origin = bar.x + ((bar.w - strip) / 2).max(0);
 600:         let limit = origin + strip - ui.px(CHIP_PAD);
 601:         let mut x = origin + ui.px(CHIP_PAD);
 602:         let mut chip = |width: i32| {
 603:             let rect = Rect::new(x, chip_y, width.min((limit - x).max(0)), chip_h);
 604:             x += width + gap;
 605:             rect
 606:         };
 607:         // Every entry is one square, active or not. A wide button carrying the active window's
 608:         // title is the Windows taskbar's signature, and it is also the redundancy this desktop
 609:         // spent a campaign removing: the window itself wears its name on its own tag, so a second
 610:         // copy of it down here says nothing new and costs 220 pixels of the switcher's room.
 611:         //
 612:         // When the surface is narrower than the chips, later cells used to clip to width 0 with
 613:         // no way to reach them. An overflow chip takes the last slot and opens the overview.
 614:         let cell_w = ui.px(CELL);
 615:         let (shown, has_overflow) =
 616:             chip_slots(bar.w, data.entries.len(), cell_w, gap, ui.px(CHIP_PAD));
 617:         let cells: Vec<Rect> = data
 618:             .entries
 619:             .iter()
 620:             .take(shown)
```

## System-cluster actions worth keeping

`shell/templar-shell/src/surface/cluster_screen.rs`

```text
  99: /// While a slider is being dragged, how often what it says actually reaches the service.
 100: ///
 101: /// Every `Set` is a round trip — one `wpctl` process for volume, one logind call for brightness —
 102: /// and a drag produces a position per frame. The pointer's own value is what the track draws, so
 103: /// throttling costs nothing visible; the release always sends, so where a drag ends is never
 104: /// approximate.
 105: const SET_THROTTLE_MS: u64 = 100;
 106: 
 107: /// The account popover's session rows, in order.
 108: ///
 109: /// Written out as literal `RunAction` constructions rather than as a table of bare ids on
 110: /// purpose: that spelling is what `xtask check-actions` reads as a pointer route, and this popover
 111: /// is the only pointer route to the session actions — Home no longer carries the Session widget.
 112: ///
 113: /// The five level rows that used to sit above these are the two sliders now. The actions they
 114: /// named have not gone anywhere and neither have their pointer routes: `volume.mute` is the volume
 115: /// slider's own label, and the four step actions are a wheel over a track.
 116: ///
 117: /// `settings.open` is not here either: it has its own chip in the cluster, and an action offered
 118: /// in two places is one a person has to look for twice.
 119: const SESSION_ROWS: [FieldAction; 5] = [
 120:     FieldAction::RunAction("session.lock"),
 121:     FieldAction::RunAction("session.suspend"),
 122:     FieldAction::RunAction("session.logout"),
 123:     FieldAction::RunAction("session.reboot"),
 124:     FieldAction::RunAction("session.poweroff"),
 125: ];
```

## Surface recreation during hiding

`shell/templar-shell/src/surface/app.rs`

```text
2891:     /// Create, resize or destroy the cluster's surface.
2892:     ///
2893:     /// Destroyed outright while a window fills the screen — the user's rule, and the honest way to
2894:     /// say it: not hidden, not collapsed to a hint, not there. Its zone is 0 like the island's, so
2895:     /// neither creating nor destroying it moves a window.
2896:     ///
2897:     /// It comes back on exactly the two things that bring the switcher back: revealing Home, and
2898:     /// the pointer resting on the bottom edge (`bar_peeked`). Without the second, the clock, the
2899:     /// network, the battery and the account would be unreachable for as long as anything was
2900:     /// maximized — which is most of the time — and "not there" would have meant "gone".
2901:     fn ensure_cluster(
2902:         &mut self,
2903:         event_loop: &ActiveEventLoop,
2904:         toplevels: &[templar_window::ShellToplevel],
2905:     ) {
2906:         let wanted_gone = bar_covered(
2907:             self.bridge.as_ref().is_some_and(ShellBridge::home_revealed),
2908:             toplevels,
2909:         ) && !self.bridge.as_ref().is_some_and(ShellBridge::bar_peeked);
2910:         if wanted_gone {
2911:             if let (Some(bridge), Some(hosted)) = (&mut self.bridge, self.cluster.take()) {
2912:                 bridge.forget_surface(&hosted.window);
2913:             }
2914:             self.cluster_wanted = (0, 0);
2915:             // And with it whatever was open on it. The screen's state does not survive the screen:
2916:             // a popover left open when something went fullscreen would otherwise be waiting when
2917:             // the window closed, over a desktop nobody had asked it about.
2918:             self.cluster_screen = crate::surface::cluster_screen::ClusterScreen::default();
2919:             return;
2920:         }
2921:         if self.cluster.is_some() {
2922:             return;
2923:         }
2924:         let Some(bridge) = &self.bridge else {
2925:             return;
2926:         };
2927:         let wanted = crate::surface::cluster_screen::wanted_size(false, self.output_width());
2928:         let window = match event_loop.create_window(
2929:             Window::default_attributes()
2930:                 .with_title("Cluster")
2931:                 .with_decorations(false)
2932:                 .with_transparent(true)
2933:                 .with_inner_size(LogicalSize::new(f64::from(wanted.0), f64::from(wanted.1))),
2934:         ) {
2935:             Ok(window) => std::sync::Arc::new(window),
2936:             Err(error) => {
2937:                 tracing::warn!(%error, "creating the cluster window");
2938:                 return;
2939:             }
```

## Existing alpha/input-region implementation and skipped presents

`shell/templar-shell/src/wayland_bridge.rs`

```text
1590:     /// Attach an `Argb8888` buffer of `colour`, with its alpha taken from `coverage`, to
1591:     /// `window`'s own surface, so a shell surface can be a shape rather than a rectangle.
1592:     ///
1593:     /// The window stays winit's: it keeps its xdg role, its configure handling and every pointer
1594:     /// and keyboard event, and only the pixels come from here. That is deliberate rather than
1595:     /// convenient — `templar_shell_v1.set_layer_surface` only *tags* a surface with a layer and
1596:     /// grants no role of its own, so a surface created on this connection instead of winit's
1597:     /// would never map. The alternative was a compositor-side role, which is a much larger
1598:     /// change for the same pixels.
1599:     ///
1600:     /// The opaque region is cleared on every attach: without it the compositor is entitled to
1601:     /// skip everything behind the surface, which is precisely the desktop the transparency is
1602:     /// there to show.
1603:     ///
1604:     // ponytail: one buffer, reused by size, attached without waiting for `wl_buffer.release` —
1605:     // the same simplification `session_lock` makes, and the same reason (one surface, one frame
1606:     // in flight, redrawn only when something changed). A double buffer is the upgrade if a
1607:     // frame ever tears.
1608:     /// Drop the masked buffer for `window`, if any. Called when the host destroys a surface so
```

```text
1619:     pub fn present_masked(
1620:         &mut self,
1621:         window: &winit::window::Window,
1622:         colour: &[u32],
1623:         coverage: &[u32],
1624:         shapes: &[(i32, i32, i32, i32)],
1625:         width: u32,
1626:         height: u32,
1627:     ) -> Result<()> {
1628:         if width == 0 || height == 0 {
1629:             bail!("a surface with no pixels");
1630:         }
1631:         let surface = self.wrap_surface(window)?;
1632:         let qh = self.queue.handle();
1633:         let shm = self
1634:             .state
1635:             .previews
1636:             .shm()
1637:             .cloned()
1638:             .ok_or_else(|| anyhow!("the compositor offers no wl_shm on this connection"))?;
1639:         let key = surface.id();
1640:         let reuse = self
1641:             .masked
1642:             .get(&key)
1643:             .is_some_and(|buffer| buffer.width == width && buffer.height == height);
1644:         if !reuse {
1645:             if let Some(old) = self.masked.remove(&key) {
1646:                 old.destroy();
1647:             }
1648:             self.masked.insert(
1649:                 key.clone(),
1650:                 crate::previews::allocate(
1651:                     &shm,
1652:                     width,
1653:                     height,
1654:                     wayland_client::protocol::wl_shm::Format::Argb8888,
1655:                     &qh,
1656:                 )
1657:                 .context("allocating the masked surface buffer")?,
1658:             );
1659:         }
1660:         let buffer = self.masked.get(&key).expect("allocated above");
1661:         if !buffer.released() {
1662:             // The compositor still holds the attached buffer; rewriting it is a protocol error
1663:             // and the frame would tear. Skip this present — the next one after `wl_buffer.release`
1664:             // will go out.
1665:             return Ok(());
1666:         }
1667:         buffer.mark_busy();
1668:         buffer.write_masked(colour, coverage)?;
1669:         surface.set_opaque_region(None);
1670:         self.declare_input_region(&surface, &qh, shapes);
1671:         surface.attach(Some(&buffer.buffer), 0, 0);
1672:         surface.damage_buffer(0, 0, width as i32, height as i32);
1673:         surface.commit();
1674:         self.connection
1675:             .flush()
1676:             .context("flushing a masked surface frame")
1677:     }
1678: 
```

```text
1681:     /// The region belongs to *presenting*, not to floating. `wl_surface.set_input_region` is
1682:     /// sticky: a frame that does not declare one keeps whatever the last frame said. So a surface
1683:     /// that fell back to an opaque present — no `wl_shm`, an older compositor, a frame with
1684:     /// nothing solid on it — kept a region describing shapes that are no longer there, at a size
1685:     /// the surface may no longer be. That is a surface taking clicks where it draws nothing and
1686:     /// refusing them where it does, and it survives until the next frame that happens to float.
1687:     ///
1688:     /// An empty `shapes` is not "no opinion", it is "none of it": a surface with nothing solid on
1689:     /// it takes no pointer at all, which is what lets the desktop underneath have it.
1690:     pub fn declare_input_shapes(
1691:         &mut self,
1692:         window: &winit::window::Window,
1693:         shapes: &[(i32, i32, i32, i32)],
1694:     ) -> Result<()> {
1695:         let surface = self.wrap_surface(window)?;
1696:         let qh = self.queue.handle();
1697:         self.declare_input_region(&surface, &qh, shapes);
1698:         surface.commit();
1699:         self.connection
1700:             .flush()
1701:             .context("flushing an input region declaration")
1702:     }
1703: 
1704:     /// The one place a region is built, so the floating path and the opaque one cannot disagree
1705:     /// about what a frame's shapes mean.
1706:     ///
1707:     /// Without a region the surface's whole rectangle takes the pointer: a click on the desktop
1708:     /// *between* two chips would be swallowed by a bar that is not there, and the compositor would
1709:     /// read the pointer as being on the shell's own chrome — which is what suppresses the hot
1710:     /// corner and holds a fullscreen peek open.
1711:     fn declare_input_region(
1712:         &self,
1713:         surface: &WlSurface,
1714:         qh: &QueueHandle<BridgeState>,
1715:         shapes: &[(i32, i32, i32, i32)],
1716:     ) {
1717:         let Some(compositor) = self.state.session_lock.compositor() else {
1718:             return;
1719:         };
1720:         let region = compositor.create_region(qh, ());
1721:         for (x, y, width, height) in shapes {
1722:             region.add(*x, *y, *width, *height);
1723:         }
1724:         surface.set_input_region(Some(&region));
1725:         region.destroy();
1726:     }
```

## Buffer release handler

`shell/templar-shell/src/previews.rs`

```text
 582: 
 583: impl Dispatch<WlBuffer, Arc<AtomicBool>> for BridgeState {
 584:     fn event(
 585:         _state: &mut Self,
 586:         _proxy: &WlBuffer,
 587:         event: wl_buffer::Event,
 588:         released: &Arc<AtomicBool>,
 589:         _conn: &Connection,
 590:         _qh: &QueueHandle<Self>,
 591:     ) {
 592:         if matches!(event, wl_buffer::Event::Release) {
 593:             released.store(true, Ordering::Release);
 594:         }
 595:     }
 596: }
```

## Shared generation currently redraws several surfaces

`shell/templar-shell/src/surface/app.rs`

```text
4946:         self.ensure_bar(event_loop, &toplevels);
4947:         self.ensure_cluster(event_loop, &toplevels);
4948:         self.sync_taskfield(event_loop);
4949:         // The window list moved: Home's session tiles and the bar both show it, so both redraw
4950:         // now rather than on the next slow sweep.
4951:         let generation = self.bridge.as_ref().map_or(0, ShellBridge::generation);
4952:         if generation != self.bridge_generation {
4953:             self.bridge_generation = generation;
4954:             if let Some(home) = &self.home {
4955:                 home.request_redraw();
4956:             }
4957:             if let Some(bar) = &self.bar {
4958:                 bar.request_redraw();
4959:             }
4960:             // Every `taskfield_state` bumps the generation too: the query, the selection and the
4961:             // filter all arrive that way, and every one of them changes what this draws.
4962:             if let Some(taskfield) = &self.taskfield {
4963:                 taskfield.request_redraw();
4964:             }
4965:         }
4966:         if self
```

## Existing private shell protocol

`interfaces/wayland/templar-shell-v1.xml`

```text
   1: <?xml version="1.0" encoding="UTF-8"?>
   2: <!-- Private, authenticated protocol between the compositor core (templar-comp) and the restartable
   3:      shell (templar-shell). Listing uses ext-foreign-toplevel-list-v1 / cosmic-toplevel-info; this
   4:      protocol carries the *commands* only the trusted shell connection may issue, and the
   5:      sanitized toplevel data (canonical app id, instance id, verified flag). Draft. -->
   6: <protocol name="templar_shell_v1">
   7:   <copyright>Copyright 2026 TemplarOS contributors. AGPL-3.0-only.</copyright>
   8: 
   9:   <interface name="templar_shell_manager_v1" version="8">
  10:     <description summary="shell control entry point">
  11:       Bound only by the client the compositor itself launched as the shell.
  12:     </description>
```

```text
 117:       <entry name="show" value="0"/>
 118:       <entry name="hide" value="1"/>
 119:       <entry name="toggle" value="2"/>
 120:     </enum>
 121:     <event name="bar_peeked" since="7">
 122:       <description summary="whether the pointer is asking for this output's bottom edge">
 123:         Sent on bind and after every state change. While set, a surface anchored to that output's
 124:         bottom edge has been asked for by a pointer resting there, under a window that covers the
 125:         screen (maximized or fullscreen); it is the surface's own business what to show, and
 126:         collapsing to a hint until this arrives is the expected shape.
 127: 
 128:         One output at a time, because there is one pointer. The empty string means no output is
 129:         being asked for.
 130:       </description>
 131:       <arg name="output" type="string" summary="the output's name, or empty for none"/>
 132:     </event>
 133:     <request name="arrange_windows" since="3">
 134:       <description summary="arrange every eligible open window per a named preset">
 135:         User-invoked arrangement, never a standing tiling policy
 136:         (`templar_window::WindowService::arrange_windows`). The first request retains each eligible
```

## Pinned Smithay dependency

`compositor/templar-comp/Cargo.toml`

```text
  58: # (see src/space_sync.rs).
  59: [dependencies.smithay]
  60: git = "https://github.com/smithay/smithay.git"
  61: rev = "1ed69cb"
  62: default-features = false
  63: features = [
  64:     "desktop",
  65:     "wayland_frontend",
  66:     "backend_egl",
  67:     "backend_winit",
  68:     "renderer_gl",
  69:     "backend_drm",
  70:     "backend_gbm",
```
