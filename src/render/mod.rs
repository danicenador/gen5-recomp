mod map_model;

use crate::Result;
use crate::formats::nitro::texture::{NitroTexture, parse_texture_archive};
use crate::game::bw2::map::ZoneMap;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct PreviewReport {
    pub output: PathBuf,
    pub triangle_count: usize,
    pub model_count: usize,
    pub model_ids: Vec<u32>,
}

#[derive(Debug, Clone)]
pub struct SceneTriangle {
    pub positions: [[f32; 3]; 3],
    pub colors: [[f32; 4]; 3],
    /// Nitro UVs are in texels; the renderer normalizes them using the material texture size.
    pub uvs: [[f32; 2]; 3],
    pub texture_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MapScene {
    pub triangles: Vec<SceneTriangle>,
    pub textures: Vec<NitroTexture>,
}

/// Load the original map-model geometry in a renderer-neutral form.
/// Game logic remains independent of wgpu and graphics resource types.
pub fn load_map_scene(cache_root: &Path, zone: &ZoneMap) -> Result<MapScene> {
    let romfs = if cache_root.join("romfs").is_dir() {
        cache_root.join("romfs")
    } else {
        cache_root.to_path_buf()
    };
    let archive_bytes = fs::read(romfs.join("a/0/0/8"))
        .map_err(|error| format!("could not read BW2 map-model archive: {error}"))?;
    let archive = crate::formats::narc::NarcArchive::parse(&archive_bytes)?;
    let mut output = Vec::new();
    let mut model_cache = HashMap::new();
    for cell_z in 0..usize::from(zone.height) {
        for cell_x in 0..usize::from(zone.width) {
            let Some(model_id) = zone.model_at(cell_x, cell_z) else {
                continue;
            };
            if !model_cache.contains_key(&model_id) {
                let index = usize::try_from(model_id)
                    .map_err(|_| format!("map model ID {model_id} does not fit this platform"))?;
                let model = map_model::read_model(archive.member(index)?)?;
                model_cache.insert(model_id, model);
            }
            let model = model_cache.get(&model_id).expect("inserted above");
            let mut triangles = map_model::scene_triangles(&model.triangles);
            for triangle in &mut triangles {
                for point in &mut triangle.positions {
                    point[0] += cell_x as f32 * 8.0;
                    point[2] += cell_z as f32 * 8.0;
                }
            }
            output.extend(triangles);
        }
    }
    if output.is_empty() {
        return Err(format!(
            "zone {} contains no renderable geometry",
            zone.zone_id
        ));
    }
    let texture_bytes = fs::read(romfs.join("a/0/1/4"))
        .map_err(|error| format!("could not read BW2 map-texture archive: {error}"))?;
    let texture_archive = crate::formats::narc::NarcArchive::parse(&texture_bytes)?;
    let texture_member = texture_archive.member(usize::from(zone.area_id))?;
    let textures = parse_texture_archive(texture_member)?;
    let available = textures
        .iter()
        .map(|texture| texture.name.as_str())
        .collect::<HashSet<_>>();
    let referenced = output
        .iter()
        .filter_map(|triangle| triangle.texture_name.as_deref())
        .collect::<HashSet<_>>();
    let missing = referenced
        .iter()
        .filter(|name| !available.contains(**name))
        .count();
    tracing::info!(
        zone = zone.zone_id,
        decoded_textures = textures.len(),
        missing_material_textures = missing,
        triangles = output.len(),
        "loaded textured map scene"
    );
    Ok(MapScene {
        triangles: output,
        textures,
    })
}

/// Render map model geometry to an SVG preview. This first renderer preserves
/// model and vertex colors; external Nitro textures are a later graphics step.
pub fn render_map_svg(cache_root: &Path, zone: &ZoneMap, output: &Path) -> Result<PreviewReport> {
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
    for cell_z in 0..usize::from(zone.height) {
        for cell_x in 0..usize::from(zone.width) {
            let Some(model_id) = zone.model_at(cell_x, cell_z) else {
                continue;
            };
            let model_id_usize = usize::try_from(model_id)
                .map_err(|_| format!("map model ID {model_id} does not fit this platform"))?;
            let member = narc.member(model_id_usize)?;
            let mut model = map_model::read_model(member)?;
            map_model::translate(
                &mut model.triangles,
                cell_x as f64 * 8.0,
                cell_z as f64 * 8.0,
            );
            triangles.extend(model.triangles);
            model_ids.push(model_id);
        }
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
