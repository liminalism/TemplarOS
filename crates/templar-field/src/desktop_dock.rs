//! Independent Desktop shelf migration and stable default placement.
use crate::{
    Cell, FieldDocument, FieldError, FieldOp, Located, Placement, Span, SpanRange, Tile, TileId,
    TileKind, apply,
};
use std::path::Path;

/// Rebind exactly one Desktop staging shelf to the authority's current XDG Desktop path.
/// Former Desktop tiles retain their id, location, dimensions and user order. No files move.
pub fn ensure_desktop_dock(
    doc: &mut FieldDocument,
    desktop: &Path,
) -> Result<(TileId, bool), FieldError> {
    let historic = TileId::for_key("folder:desktop");
    let candidates: Vec<TileId> = doc
        .tiles
        .iter()
        .filter(|tile| match &tile.kind {
            TileKind::DesktopDock { .. } => true,
            TileKind::Folder { path } => {
                tile.id == historic || (path == desktop && doc.parent_of(tile.id).is_none())
            }
            _ => false,
        })
        .map(|tile| tile.id)
        .collect();
    let id = candidates
        .iter()
        .copied()
        .find(|id| {
            matches!(
                doc.tile(*id).map(|t| &t.kind),
                Some(TileKind::DesktopDock { .. })
            )
        })
        .or_else(|| candidates.iter().copied().find(|id| *id == historic))
        .or_else(|| candidates.first().copied())
        .unwrap_or(historic);
    if candidates.is_empty() {
        let kind = TileKind::DesktopDock {
            path: desktop.to_path_buf(),
            order: Vec::new(),
        };
        let mut spans = SpanRange::for_kind(&kind, doc.cols, doc.rows);
        spans.min = Span::new(spans.min.cols.min(doc.cols), spans.min.rows.min(doc.rows));
        spans.preferred = Span::new(
            spans.preferred.cols.min(doc.cols),
            spans.preferred.rows.min(doc.rows),
        );
        // Bottom/right edge, away from the Home corner; do not displace other user objects.
        let at = (0..doc.rows)
            .rev()
            .flat_map(|row| (0..doc.cols).rev().map(move |col| Cell::new(col, row)))
            .map(|cell| Placement::new(cell, spans.preferred))
            .find(|placement| doc.is_free(None, *placement, None))
            .map(|placement| Located {
                container: None,
                placement,
            });
        let tile = Tile {
            id,
            kind,
            title: "Desktop".into(),
            spans,
            accent: None,
        };
        *doc = apply(
            doc,
            &FieldOp::AddTile {
                tile,
                at,
                container: None,
            },
        )?
        .0;
        return Ok((id, true));
    }
    let mut order = Vec::new();
    for candidate in &candidates {
        if let Some(TileKind::DesktopDock { order: old, .. }) =
            doc.tile(*candidate).map(|t| &t.kind)
        {
            for path in old {
                if !order.contains(path) {
                    order.push(path.clone());
                }
            }
        }
    }
    let desired = TileKind::DesktopDock {
        path: desktop.to_path_buf(),
        order,
    };
    let mut changed = candidates.len() > 1 || doc.tile(id).is_some_and(|tile| tile.kind != desired);
    doc.tile_mut(id).ok_or(FieldError::UnknownTile)?.kind = desired;
    // Geometry survives even if it was outside the new class range on an older build.
    let location = doc.location(id);
    let cols = doc.cols;
    let rows = doc.rows;
    let tile = doc.tile_mut(id).ok_or(FieldError::UnknownTile)?;
    let mut range = SpanRange::for_kind(&tile.kind, cols, rows);
    if let Some(location) = location {
        range.min = Span::new(
            range.min.cols.min(location.placement.span.cols),
            range.min.rows.min(location.placement.span.rows),
        );
        range.max = Span::new(
            range.max.cols.max(location.placement.span.cols),
            range.max.rows.max(location.placement.span.rows),
        );
        range.preferred = location.placement.span;
    }
    changed |= tile.spans != range;
    tile.spans = range;
    for duplicate in candidates.into_iter().filter(|candidate| *candidate != id) {
        *doc = apply(doc, &FieldOp::Remove { id: duplicate })?.0;
        for panel in &mut doc.panels {
            for member in &mut panel.members {
                if *member == duplicate {
                    *member = id;
                }
            }
            let mut seen = std::collections::BTreeSet::new();
            panel.members.retain(|member| seen.insert(*member));
            if let Some(baseline) = &mut panel.baseline {
                baseline.retain(|(member, _)| *member != duplicate);
            }
        }
    }
    Ok((id, changed))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn migrating_desktop_preserves_geometry_and_leaves_other_folders() {
        let mut doc = FieldDocument::new("primary", 12, 8);
        let id = TileId::for_key("folder:desktop");
        let at = Located {
            container: None,
            placement: Placement::new(Cell::new(2, 3), Span::new(3, 3)),
        };
        doc = apply(
            &doc,
            &FieldOp::AddTile {
                tile: crate::folder_tile(id, Path::new("/home/a/OldDesktop"), 12, 8),
                at: Some(at),
                container: None,
            },
        )
        .unwrap()
        .0;
        let other = TileId(9);
        doc = apply(
            &doc,
            &FieldOp::AddTile {
                tile: crate::folder_tile(other, Path::new("/home/a/Documents"), 12, 8),
                at: None,
                container: None,
            },
        )
        .unwrap()
        .0;
        assert_eq!(
            ensure_desktop_dock(&mut doc, Path::new("/home/a/Bureau")).unwrap(),
            (id, true)
        );
        assert_eq!(doc.location(id), Some(at));
        assert!(
            matches!(&doc.tile(id).unwrap().kind, TileKind::DesktopDock { path, .. } if path == Path::new("/home/a/Bureau"))
        );
        assert!(matches!(
            doc.tile(other).unwrap().kind,
            TileKind::Folder { .. }
        ));
        assert_eq!(
            ensure_desktop_dock(&mut doc, Path::new("/home/a/Bureau")).unwrap(),
            (id, false)
        );
    }
    #[test]
    fn dock_order_is_undoable_and_does_not_touch_placement() {
        let mut doc = FieldDocument::new("primary", 12, 8);
        let (id, _) = ensure_desktop_dock(&mut doc, Path::new("/home/a/Desktop")).unwrap();
        let (changed, undo) = apply(
            &doc,
            &FieldOp::DesktopDockOrder {
                id,
                order: vec!["L2E=".into()],
            },
        )
        .unwrap();
        assert_eq!(changed.location(id), doc.location(id));
        assert_eq!(apply(&changed, &undo).unwrap().0, doc);
    }
    #[test]
    fn new_dock_defaults_to_opposite_edge_without_moving_neighbors() {
        let mut doc = FieldDocument::new("primary", 12, 8);
        let (id, _) = ensure_desktop_dock(&mut doc, Path::new("/home/a/Desktop")).unwrap();
        let placement = doc.location(id).unwrap().placement;
        assert_eq!((placement.right(), placement.bottom()), (12, 8));
        assert!(!crate::garden::is_garden_tile(
            doc.tile(id).unwrap(),
            &["/home/a/Desktop".into()]
        ));
    }
    #[test]
    fn raw_desktop_and_reference_paths_survive_document_and_inverse_persistence() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt, path::PathBuf};
        let desktop = PathBuf::from(OsString::from_vec(b"/home/name-\xff/Desktop".to_vec()));
        let file = desktop.join(OsString::from_vec(b"file-\xfe".to_vec()));
        let mut doc = FieldDocument::new("primary", 12, 8);
        ensure_desktop_dock(&mut doc, &desktop).unwrap();
        doc.trays.push(crate::Tray {
            id: crate::TrayId(7),
            name: "References".into(),
            entries: vec![file.clone()],
        });
        let persisted = serde_json::to_string(&doc).unwrap();
        assert_eq!(
            serde_json::from_str::<FieldDocument>(&persisted).unwrap(),
            doc
        );
        let operation = FieldOp::RemoveReference {
            tray: crate::TrayId(7),
            path: file,
        };
        let (removed, inverse) = apply(&doc, &operation).unwrap();
        let persisted_inverse = serde_json::to_string(&inverse).unwrap();
        let loaded_inverse: FieldOp = serde_json::from_str(&persisted_inverse).unwrap();
        assert_eq!(apply(&removed, &loaded_inverse).unwrap().0, doc);
    }
}
