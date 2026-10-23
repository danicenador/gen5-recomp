mod desktop;
mod wgpu_renderer;

use crate::Result;
use crate::game::bw2::field::FieldSession;
use crate::game::bw2::map::ZoneMap;
use crate::render;
use std::path::Path;

/// Launch the native desktop field slice using the imported map geometry.
pub fn run(cache_root: &Path, zone: ZoneMap, start: Option<(usize, usize)>) -> Result<()> {
    let session = FieldSession::load(cache_root, zone.clone(), start)?;
    let scene = render::load_map_scene(cache_root, &zone)?;
    desktop::run(cache_root, session, scene)
}
