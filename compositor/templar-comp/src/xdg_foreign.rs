//! Standard xdg-foreign v2 presentation-only capabilities.
//!
//! Smithay's pinned delegate retains only one child per export and does not revoke a dead
//! exported surface. Keep the standard wire interface, with all import/child lifetimes here.
use crate::state::{Backend, TemplarCompState};
use smithay::{
    reexports::{
        wayland_protocols::xdg::foreign::zv2::server::{
            zxdg_exported_v2::{self, ZxdgExportedV2},
            zxdg_exporter_v2::{self, ZxdgExporterV2},
            zxdg_imported_v2::{self, ZxdgImportedV2},
            zxdg_importer_v2::{self, ZxdgImporterV2},
        },
        wayland_server::{
            Client, DataInit, DisplayHandle, New, Resource, backend::ClientId,
            protocol::wl_surface::WlSurface,
        },
    },
    wayland::{
        Dispatch2, GlobalDispatch2,
        compositor::{get_role, with_states},
        shell::{
            is_valid_parent,
            xdg::{XDG_TOPLEVEL_ROLE, XdgToplevelSurfaceData},
        },
    },
};
use std::collections::HashMap;

#[derive(Debug, Default)]
pub struct ForeignState {
    exports: HashMap<String, (WlSurface, ZxdgExportedV2)>,
    imports: HashMap<ZxdgImportedV2, String>,
    children: HashMap<WlSurface, ZxdgImportedV2>,
}
#[derive(Debug)]
pub struct Exporter;
#[derive(Debug)]
pub struct Importer;
#[derive(Debug)]
pub struct Exported(String);
#[derive(Debug)]
pub struct Imported;

pub fn create_globals<B: Backend + 'static>(dh: &DisplayHandle) {
    dh.create_global::<TemplarCompState<B>, ZxdgExporterV2, _>(1, Exporter);
    dh.create_global::<TemplarCompState<B>, ZxdgImporterV2, _>(1, Importer);
}

fn set_parent<B: Backend + 'static>(
    state: &mut TemplarCompState<B>,
    child: &WlSurface,
    parent: Option<WlSurface>,
) {
    with_states(child, |states| {
        if let Some(data) = states.data_map.get::<XdgToplevelSurfaceData>() {
            data.lock().unwrap().parent = parent;
        }
    });
    sync_parent(state, child);
}

/// Both same-connection xdg-shell and foreign parenting enter the same policy boundary.
pub fn sync_parent<B: Backend + 'static>(state: &mut TemplarCompState<B>, child: &WlSurface) {
    let parent = with_states(child, |states| {
        states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .and_then(|data| data.lock().unwrap().parent.clone())
    });
    if let Some(imported) = state.foreign_parent.children.get(child) {
        let exported_parent = state
            .foreign_parent
            .imports
            .get(imported)
            .and_then(|handle| state.foreign_parent.exports.get(handle))
            .map(|(surface, _)| surface);
        if exported_parent != parent.as_ref() {
            state.foreign_parent.children.remove(child);
        }
    }
    let Some(child_id) = state.windows.window_id_for(&child.id()) else {
        return;
    };
    let parent_id = parent
        .as_ref()
        .filter(|parent| parent.is_alive())
        .and_then(|parent| state.windows.window_id_for(&parent.id()));
    if !state
        .window_service
        .set_transient_parent(child_id, parent_id)
    {
        state.window_service.set_transient_parent(child_id, None);
        tracing::warn!(
            ?child_id,
            "transient parent rejected by application-role policy; dialog remains independent"
        );
    }
    tracing::info!(?child_id, ?parent_id, "transient presentation binding");
    crate::space_sync::sync_after_change(state);
}

/// A child may set its parent before either window has mapped.
pub fn mapped<B: Backend + 'static>(state: &mut TemplarCompState<B>, parent: &WlSurface) {
    sync_parent(state, parent);
    for index in 0..state.xdg_shell_state.toplevel_surfaces().len() {
        let child = state.xdg_shell_state.toplevel_surfaces()[index]
            .wl_surface()
            .clone();
        let related = with_states(&child, |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .is_some_and(|data| data.lock().unwrap().parent.as_ref() == Some(parent))
        });
        if related {
            sync_parent(state, &child);
        }
    }
}

fn detach_import<B: Backend + 'static>(state: &mut TemplarCompState<B>, imported: &ZxdgImportedV2) {
    let children: Vec<_> = state
        .foreign_parent
        .children
        .iter()
        .filter(|(_, owner)| *owner == imported)
        .map(|(child, _)| child.clone())
        .collect();
    for child in children {
        state.foreign_parent.children.remove(&child);
        if child.is_alive() {
            set_parent(state, &child, None);
        }
    }
}
fn revoke<B: Backend + 'static>(state: &mut TemplarCompState<B>, handle: &str) {
    let imports: Vec<_> = state
        .foreign_parent
        .imports
        .iter()
        .filter(|(_, key)| key.as_str() == handle)
        .map(|(imported, _)| imported.clone())
        .collect();
    for imported in imports {
        state.foreign_parent.imports.remove(&imported);
        if imported.is_alive() {
            imported.destroyed();
        }
        detach_import(state, &imported);
    }
    state.foreign_parent.exports.remove(handle);
}

/// A surface can die while its xdg_exported object remains alive.
pub fn surface_destroyed<B: Backend + 'static>(
    state: &mut TemplarCompState<B>,
    surface: &WlSurface,
) {
    let handles: Vec<_> = state
        .foreign_parent
        .exports
        .iter()
        .filter(|(_, (parent, _))| parent == surface)
        .map(|(handle, _)| handle.clone())
        .collect();
    for handle in handles {
        revoke(state, &handle);
    }
    state.foreign_parent.children.remove(surface);
    // Same-client xdg_toplevel parents have no import resource to revoke.
    let children: Vec<_> = state
        .xdg_shell_state
        .toplevel_surfaces()
        .iter()
        .filter_map(|toplevel| {
            let child = toplevel.wl_surface();
            with_states(child, |states| {
                states
                    .data_map
                    .get::<XdgToplevelSurfaceData>()
                    .is_some_and(|data| data.lock().unwrap().parent.as_ref() == Some(surface))
            })
            .then(|| child.clone())
        })
        .collect();
    for child in children {
        set_parent(state, &child, None);
    }
}

impl<B: Backend + 'static> GlobalDispatch2<ZxdgExporterV2, TemplarCompState<B>> for Exporter {
    fn bind(
        &self,
        _: &mut TemplarCompState<B>,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ZxdgExporterV2>,
        init: &mut DataInit<'_, TemplarCompState<B>>,
    ) {
        init.init(resource, Exporter);
    }
}
impl<B: Backend + 'static> GlobalDispatch2<ZxdgImporterV2, TemplarCompState<B>> for Importer {
    fn bind(
        &self,
        _: &mut TemplarCompState<B>,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ZxdgImporterV2>,
        init: &mut DataInit<'_, TemplarCompState<B>>,
    ) {
        init.init(resource, Importer);
    }
}
impl<B: Backend + 'static> Dispatch2<ZxdgExporterV2, TemplarCompState<B>> for Exporter {
    fn request(
        &self,
        state: &mut TemplarCompState<B>,
        client: &Client,
        resource: &ZxdgExporterV2,
        request: zxdg_exporter_v2::Request,
        _: &DisplayHandle,
        init: &mut DataInit<'_, TemplarCompState<B>>,
    ) {
        if let zxdg_exporter_v2::Request::ExportToplevel { id, surface } = request {
            if surface.client().as_ref() != Some(client)
                || get_role(&surface) != Some(XDG_TOPLEVEL_ROLE)
            {
                resource.post_error(
                    zxdg_exporter_v2::Error::InvalidSurface,
                    "export requires own xdg_toplevel",
                );
                return;
            }
            let handle = uuid::Uuid::new_v4().simple().to_string();
            let exported = init.init(id, Exported(handle.clone()));
            exported.handle(handle.clone());
            state
                .foreign_parent
                .exports
                .insert(handle, (surface, exported));
        }
    }
}
impl<B: Backend + 'static> Dispatch2<ZxdgExportedV2, TemplarCompState<B>> for Exported {
    fn request(
        &self,
        _: &mut TemplarCompState<B>,
        _: &Client,
        _: &ZxdgExportedV2,
        _: zxdg_exported_v2::Request,
        _: &DisplayHandle,
        _: &mut DataInit<'_, TemplarCompState<B>>,
    ) {
    }
    fn destroyed(&self, state: &mut TemplarCompState<B>, _: ClientId, _: &ZxdgExportedV2) {
        revoke(state, &self.0);
    }
}
impl<B: Backend + 'static> Dispatch2<ZxdgImporterV2, TemplarCompState<B>> for Importer {
    fn request(
        &self,
        state: &mut TemplarCompState<B>,
        _: &Client,
        _: &ZxdgImporterV2,
        request: zxdg_importer_v2::Request,
        _: &DisplayHandle,
        init: &mut DataInit<'_, TemplarCompState<B>>,
    ) {
        if let zxdg_importer_v2::Request::ImportToplevel { id, handle } = request {
            let imported = init.init(id, Imported);
            if handle.len() <= 256
                && state
                    .foreign_parent
                    .exports
                    .get(&handle)
                    .is_some_and(|(surface, _)| surface.is_alive())
            {
                state.foreign_parent.imports.insert(imported, handle);
            } else {
                imported.destroyed();
            }
        }
    }
}
impl<B: Backend + 'static> Dispatch2<ZxdgImportedV2, TemplarCompState<B>> for Imported {
    fn request(
        &self,
        state: &mut TemplarCompState<B>,
        client: &Client,
        resource: &ZxdgImportedV2,
        request: zxdg_imported_v2::Request,
        _: &DisplayHandle,
        _: &mut DataInit<'_, TemplarCompState<B>>,
    ) {
        if let zxdg_imported_v2::Request::SetParentOf { surface: child } = request {
            if child.client().as_ref() != Some(client)
                || get_role(&child) != Some(XDG_TOPLEVEL_ROLE)
            {
                resource.post_error(
                    zxdg_imported_v2::Error::InvalidSurface,
                    "parenting requires own xdg_toplevel child",
                );
                return;
            }
            let parent = state
                .foreign_parent
                .imports
                .get(resource)
                .and_then(|handle| state.foreign_parent.exports.get(handle))
                .map(|(parent, _)| parent.clone());
            let Some(parent) = parent.filter(|parent| parent.is_alive()) else {
                return;
            };
            if !is_valid_parent(&child, &parent) {
                resource.post_error(
                    zxdg_imported_v2::Error::InvalidSurface,
                    "cyclic parent relationship",
                );
                return;
            }
            state
                .foreign_parent
                .children
                .insert(child.clone(), resource.clone());
            set_parent(state, &child, Some(parent));
        }
    }
    fn destroyed(&self, state: &mut TemplarCompState<B>, _: ClientId, resource: &ZxdgImportedV2) {
        state.foreign_parent.imports.remove(resource);
        detach_import(state, resource);
    }
}
