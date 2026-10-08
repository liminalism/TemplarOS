//! Linux no-clobber rename for native file paths. A prior existence check is not a collision
//! guarantee: another client may create the destination between that check and the rename.
use std::{io, path::Path};

pub fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    rustix::fs::renameat_with(
        rustix::fs::CWD,
        from,
        rustix::fs::CWD,
        to,
        rustix::fs::RenameFlags::NOREPLACE,
    )
    .map_err(io::Error::from)
}
