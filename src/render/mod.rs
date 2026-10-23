mod map_model;

use crate::Result;
use crate::game::bw2::map::ZoneMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct PreviewReport {
    pub output: PathBuf,
    pub triangle_count: usize,
    pub model_count: usize,
    pub model_ids: Vec<u32>,
}

/// Render map model geometry to an SVG preview. This first renderer preserves
/// model and vertex colors; external Nitro textures are a later graphics step.
pub fn render_map_svg(cache_root: &Path, zone: &ZoneMap, output: &Path) -> Result<PreviewReport> {
    if zone.width != 1 || zone.height != 1 || zone.model_ids.len() != 1 {
        return Err(format!(
            "map preview currently supports one-cell maps; zone {} is {}x{}",
            zone.zone_id, zone.width, zone.height
        ));
    }
    let romfs = if cache_root.join("romfs").is_dir() {
        cache_root.join("romfs")
    } else {
        cache_root.to_path_buf()
    };
    let model_archive_path = romfs.join("a/0/0/8");
    let model_archive = fs::read(&model_archive_path)
        .map_err(|error| format!("could not read BW2 map-model archive: {error}"))?;
    let narc = crate::formats::narc::NarcArchive::parse(&model_archive)?;
    let mut triangles = Vec::new();
    let mut model_ids = Vec::new();
    for model_id in zone.model_ids.iter().flatten().copied() {
        let model_id_usize = usize::try_from(model_id)
            .map_err(|_| format!("map model ID {model_id} does not fit this platform"))?;
        let member = narc.member(model_id_usize)?;
        let model = map_model::read_model(member)?;
        triangles.extend(model.triangles);
        model_ids.push(model_id);
    }
    if triangles.is_empty() {
        return Err(format!(
            "zone {} contains no renderable map triangles",
            zone.zone_id
        ));
    }
    let svg = map_model::write_svg(zone, &triangles)?;
    if let Some(parent) = output.parent().filter(|path| !path.as_os_str().is_empty()) {
        fs::create_dir_all(parent)
            .map_err(|error| format!("could not create preview directory: {error}"))?;
    }
    fs::write(output, svg).map_err(|error| format!("could not write map preview: {error}"))?;
    Ok(PreviewReport {
        output: output.to_path_buf(),
        triangle_count: triangles.len(),
        model_count: model_ids.len(),
        model_ids,
    })
}
