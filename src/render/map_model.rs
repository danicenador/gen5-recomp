use crate::Result;
use crate::game::bw2::map::ZoneMap;

#[derive(Debug, Clone, Copy)]
struct Vertex {
    position: [f64; 3],
    color: [u8; 3],
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Triangle {
    vertices: [Vertex; 3],
    material_color: Option<[u8; 3]>,
    depth: f64,
}

#[derive(Debug)]
pub(super) struct ModelGeometry {
    pub triangles: Vec<Triangle>,
}

pub(super) fn scene_triangles(triangles: &[Triangle]) -> Vec<super::SceneTriangle> {
    triangles
        .iter()
        .map(|triangle| {
            let positions = triangle.vertices.map(|vertex| {
                [
                    vertex.position[0] as f32,
                    vertex.position[1] as f32,
                    vertex.position[2] as f32,
                ]
            });
            let colors = triangle.vertices.map(|vertex| {
                let color = triangle.material_color.unwrap_or(vertex.color);
                [
                    f32::from(color[0]) / 255.0,
                    f32::from(color[1]) / 255.0,
                    f32::from(color[2]) / 255.0,
                    1.0,
                ]
            });
            super::SceneTriangle { positions, colors }
        })
        .collect()
}

#[derive(Debug, Clone, Copy)]
struct Draw {
    piece: usize,
    material: usize,
    object: usize,
}

#[derive(Debug, Clone, Copy)]
struct Material {
    diffuse: [u8; 3],
    uses_vertex_color: bool,
}

#[derive(Debug, Clone, Copy)]
struct Transform {
    translation: [f64; 3],
    scale: [f64; 3],
    rotation: [[f64; 3]; 3],
}

impl Default for Transform {
    fn default() -> Self {
        Self {
            translation: [0.0; 3],
            scale: [1.0; 3],
            rotation: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
        }
    }
}

#[derive(Debug)]
struct InfoBlock {
    relative_base: usize,
    entries: Vec<(u32, String)>,
}

pub(super) fn read_model(bytes: &[u8]) -> Result<ModelGeometry> {
    let bmd_start = bytes
        .windows(4)
        .position(|window| window == b"BMD0")
        .ok_or_else(|| "map model member contains no BMD0 model".to_owned())?;
    require(bytes, bmd_start, 16, "BMD0 header")?;
    let bmd_size = usize_at_u32(bytes, bmd_start + 8)?;
    let header_size = usize::from(u16_at(bytes, bmd_start + 12)?);
    let section_count = usize::from(u16_at(bytes, bmd_start + 14)?);
    if header_size < 16 || bmd_size < header_size || bmd_start + bmd_size > bytes.len() {
        return Err("BMD0 header declares an invalid size".to_owned());
    }
    let bmd_end = bmd_start + bmd_size;
    require(
        bytes,
        bmd_start + 16,
        section_count * 4,
        "BMD0 section table",
    )?;
    let mut mdl0_range = None;
    for section_index in 0..section_count {
        let offset = usize_at_u32(bytes, bmd_start + 16 + section_index * 4)?;
        let section_start = bmd_start
            .checked_add(offset)
            .ok_or_else(|| "BMD0 section offset overflowed".to_owned())?;
        require(bytes, section_start, 8, "BMD0 section header")?;
        let section_size = usize_at_u32(bytes, section_start + 4)?;
        if section_size < 8 || section_start + section_size > bmd_end {
            return Err(format!("BMD0 section {section_index} has an invalid size"));
        }
        if &bytes[section_start..section_start + 4] == b"MDL0" {
            mdl0_range = Some((section_start, section_start + section_size));
        }
    }
    let (mdl0_start, mdl0_end) = mdl0_range.ok_or_else(|| "BMD0 has no MDL0 section".to_owned())?;
    let model_info = read_info_block(bytes, mdl0_start + 8, mdl0_start, mdl0_end)?;
    let Some((model_offset, _model_name)) = model_info.entries.first() else {
        return Err("MDL0 contains no model entries".to_owned());
    };
    let model_start = model_info
        .relative_base
        .checked_add(
            usize::try_from(*model_offset).map_err(|_| "model offset overflowed".to_owned())?,
        )
        .ok_or_else(|| "model offset overflowed".to_owned())?;
    require(bytes, model_start, 0x40, "MDL0 model header")?;
    let model_size = usize_at_u32(bytes, model_start)?;
    let model_end = model_start
        .checked_add(model_size)
        .ok_or_else(|| "MDL0 model size overflowed".to_owned())?;
    if model_size < 0x40 || model_end > mdl0_end {
        return Err("MDL0 model block has an invalid size".to_owned());
    }
    let render_offset = usize_at_u32(bytes, model_start + 4)?;
    let material_offset = usize_at_u32(bytes, model_start + 8)?;
    let pieces_offset = usize_at_u32(bytes, model_start + 12)?;
    let object_count = usize::from(bytes[model_start + 0x17]);
    let material_count = usize::from(bytes[model_start + 0x18]);
    let piece_count = usize::from(bytes[model_start + 0x19]);

    let object_table_start = model_start + 0x40;
    let objects_info = read_info_block(bytes, object_table_start, object_table_start, model_end)?;
    if objects_info.entries.len() != object_count {
        return Err("MDL0 object count does not match its info block".to_owned());
    }
    let objects = objects_info
        .entries
        .iter()
        .map(|(offset, _)| {
            let start = objects_info.relative_base + *offset as usize;
            parse_object(bytes, start, model_end)
        })
        .collect::<Result<Vec<_>>>()?;

    let materials_info = read_material_info(bytes, model_start + material_offset, model_end)?;
    if materials_info.len() != material_count {
        return Err("MDL0 material count does not match its info block".to_owned());
    }
    let piece_table_start = model_start + pieces_offset;
    let pieces_info = read_info_block(bytes, piece_table_start, piece_table_start, model_end)?;
    if pieces_info.entries.len() != piece_count {
        return Err("MDL0 piece count does not match its info block".to_owned());
    }
    let piece_commands = pieces_info
        .entries
        .iter()
        .map(|(offset, _)| {
            let start = pieces_info.relative_base + *offset as usize;
            parse_piece(bytes, start, model_end)
        })
        .collect::<Result<Vec<_>>>()?;
    let draw_commands = parse_render_commands(bytes, model_start + render_offset, model_end)?;
    if draw_commands.is_empty() {
        return Err("MDL0 render-command list contains no draw commands".to_owned());
    }

    let mut triangles = Vec::new();
    for draw in draw_commands {
        let commands = piece_commands
            .get(draw.piece)
            .ok_or_else(|| format!("MDL0 render command refers to missing piece {}", draw.piece))?;
        let transform = objects.get(draw.object).copied().unwrap_or_default();
        let material = materials_info.get(draw.material).copied();
        for mut triangle in decode_gpu_commands(commands)? {
            for vertex in &mut triangle.vertices {
                vertex.position = transform.apply(vertex.position);
            }
            triangle.material_color = material
                .filter(|entry| !entry.uses_vertex_color)
                .map(|entry| entry.diffuse);
            triangle.depth = triangle
                .vertices
                .iter()
                .map(|vertex| vertex.position[0] + vertex.position[1] + vertex.position[2])
                .sum::<f64>()
                / 3.0;
            triangles.push(triangle);
        }
    }
    Ok(ModelGeometry { triangles })
}

fn read_info_block(
    bytes: &[u8],
    start: usize,
    relative_base: usize,
    limit: usize,
) -> Result<InfoBlock> {
    require(bytes, start, 4, "Nitro info-block header")?;
    let count = usize::from(bytes[start + 1]);
    let header_size = usize::from(u16_at(bytes, start + 2)?);
    let data_start = start
        .checked_add(16 + count * 4)
        .ok_or_else(|| "Nitro info-block size overflowed".to_owned())?;
    let names_start = data_start
        .checked_add(count * 4)
        .ok_or_else(|| "Nitro info-block size overflowed".to_owned())?;
    let expected_size = 16usize
        .checked_add(count * 24)
        .ok_or_else(|| "Nitro info-block size overflowed".to_owned())?;
    let datum_size_offset = start + 12 + count * 4;
    if bytes[start] != 0 || u16_at(bytes, datum_size_offset)? != 4 || header_size != expected_size {
        return Err(format!(
            "Nitro info block at 0x{start:X} has an invalid header"
        ));
    }
    require(bytes, start, expected_size, "Nitro info block")?;
    if start + expected_size > limit {
        return Err("Nitro info block extends past its parent section".to_owned());
    }
    let mut entries = Vec::with_capacity(count);
    for index in 0..count {
        let offset = u32_at(bytes, data_start + index * 4)?;
        let name_start = names_start + index * 16;
        let name_bytes = &bytes[name_start..name_start + 16];
        let end = name_bytes.iter().position(|byte| *byte == 0).unwrap_or(16);
        let name = String::from_utf8_lossy(&name_bytes[..end]).into_owned();
        entries.push((offset, name));
    }
    Ok(InfoBlock {
        relative_base,
        entries,
    })
}

fn read_material_info(bytes: &[u8], start: usize, limit: usize) -> Result<Vec<Material>> {
    require(bytes, start, 4, "MDL0 material table")?;
    let info = read_info_block(bytes, start + 4, start, limit)?;
    info.entries
        .iter()
        .map(|(offset, _)| {
            let material_start = info.relative_base + *offset as usize;
            require(bytes, material_start, 20, "MDL0 material record")?;
            if material_start + 20 > limit {
                return Err("MDL0 material record extends past its model".to_owned());
            }
            let diffuse_ambient = u32_at(bytes, material_start + 4)?;
            let raw_color = diffuse_ambient & 0x7FFF;
            Ok(Material {
                diffuse: rgb555(raw_color),
                uses_vertex_color: diffuse_ambient & 0x8000 != 0,
            })
        })
        .collect()
}

fn parse_object(bytes: &[u8], start: usize, limit: usize) -> Result<Transform> {
    require(bytes, start, 4, "MDL0 object record")?;
    if start + 4 > limit {
        return Err("MDL0 object record extends past its model".to_owned());
    }
    let flags = u16_at(bytes, start)?;
    let translation_present = flags & 1 == 0;
    let rotation_present = (flags >> 1) & 1 == 0;
    let scale_present = (flags >> 2) & 1 == 0;
    let pivot_rotation = (flags >> 3) & 1 != 0;
    let mut cursor = start + 4;
    let mut transform = Transform::default();
    if translation_present {
        require(bytes, cursor, 12, "MDL0 object translation")?;
        transform.translation = [
            i32_at(bytes, cursor)? as f64 / 4096.0,
            i32_at(bytes, cursor + 4)? as f64 / 4096.0,
            i32_at(bytes, cursor + 8)? as f64 / 4096.0,
        ];
        cursor += 12;
    }
    if rotation_present && !pivot_rotation {
        require(bytes, cursor, 16, "MDL0 object rotation")?;
        let mut values = [0.0; 9];
        values[0] = i16_at(bytes, start + 2)? as f64 / 4096.0;
        for index in 1..9 {
            values[index] = i16_at(bytes, cursor + (index - 1) * 2)? as f64 / 4096.0;
        }
        transform.rotation = [
            [values[0], values[1], values[2]],
            [values[3], values[4], values[5]],
            [values[6], values[7], values[8]],
        ];
        cursor += 16;
    }
    if scale_present {
        require(bytes, cursor, 12, "MDL0 object scale")?;
        transform.scale = [
            i32_at(bytes, cursor)? as f64 / 4096.0,
            i32_at(bytes, cursor + 4)? as f64 / 4096.0,
            i32_at(bytes, cursor + 8)? as f64 / 4096.0,
        ];
    }
    Ok(transform)
}

impl Transform {
    fn apply(self, point: [f64; 3]) -> [f64; 3] {
        let scaled = [
            point[0] * self.scale[0],
            point[1] * self.scale[1],
            point[2] * self.scale[2],
        ];
        let rotated = [
            self.rotation[0][0] * scaled[0]
                + self.rotation[0][1] * scaled[1]
                + self.rotation[0][2] * scaled[2],
            self.rotation[1][0] * scaled[0]
                + self.rotation[1][1] * scaled[1]
                + self.rotation[1][2] * scaled[2],
            self.rotation[2][0] * scaled[0]
                + self.rotation[2][1] * scaled[1]
                + self.rotation[2][2] * scaled[2],
        ];
        [
            rotated[0] + self.translation[0],
            rotated[1] + self.translation[1],
            rotated[2] + self.translation[2],
        ]
    }
}

fn parse_piece(bytes: &[u8], start: usize, limit: usize) -> Result<Vec<u8>> {
    require(bytes, start, 16, "MDL0 piece record")?;
    if start + 16 > limit || u16_at(bytes, start + 2)? != 16 {
        return Err("MDL0 piece record has an invalid size".to_owned());
    }
    let command_offset = usize_at_u32(bytes, start + 8)?;
    let command_length = usize_at_u32(bytes, start + 12)?;
    if command_length % 4 != 0 {
        return Err("MDL0 piece command list is not 4-byte aligned".to_owned());
    }
    let command_start = start
        .checked_add(command_offset)
        .ok_or_else(|| "MDL0 piece command offset overflowed".to_owned())?;
    require(
        bytes,
        command_start,
        command_length,
        "MDL0 piece command list",
    )?;
    if command_start + command_length > limit {
        return Err("MDL0 piece command list extends past its model".to_owned());
    }
    Ok(bytes[command_start..command_start + command_length].to_vec())
}

fn parse_render_commands(bytes: &[u8], start: usize, limit: usize) -> Result<Vec<Draw>> {
    if start >= limit {
        return Err("MDL0 render-command offset is outside its model".to_owned());
    }
    let mut cursor = start;
    let mut current_material = 0usize;
    let mut current_object = 0usize;
    let mut draws = Vec::new();
    while cursor < limit {
        let opcode = bytes[cursor];
        cursor += 1;
        if opcode == 0x01 {
            return Ok(draws);
        }
        let param_count = if opcode == 0x09 {
            require(bytes, cursor, 2, "MDL0 skin command")?;
            2 + usize::from(bytes[cursor + 1]) * 3
        } else {
            match opcode {
                0x00 | 0x0B | 0x2B | 0x40 | 0x80 => 0,
                0x02 | 0x0C | 0x0D | 0x47 => 2,
                0x03 | 0x04 | 0x05 | 0x07 | 0x08 | 0x24 | 0x44 => 1,
                0x06 => 3,
                0x26 | 0x46 => 4,
                0x66 => 5,
                _ => return Err(format!("unsupported MDL0 render opcode 0x{opcode:02X}")),
            }
        };
        let param_bytes = param_count;
        require(bytes, cursor, param_bytes, "MDL0 render-command parameters")?;
        let params = &bytes[cursor..cursor + param_bytes];
        match opcode {
            0x04 | 0x24 | 0x44 => current_material = usize::from(params[0]),
            0x05 => draws.push(Draw {
                piece: usize::from(params[0]),
                material: current_material,
                object: current_object,
            }),
            0x06 | 0x26 | 0x46 | 0x66 => current_object = usize::from(params[0]),
            _ => {}
        }
        cursor += param_bytes;
    }
    Err("MDL0 render-command list has no end opcode".to_owned())
}

fn decode_gpu_commands(bytes: &[u8]) -> Result<Vec<Triangle>> {
    let mut cursor = 0usize;
    let mut current = [0.0; 3];
    let mut current_color = [255; 3];
    let mut primitive_mode = None;
    let mut primitive_vertices = Vec::new();
    let mut triangles = Vec::new();
    while cursor < bytes.len() {
        let command_word = read_gpu_word(bytes, &mut cursor)?;
        let opcodes = command_word.to_le_bytes();
        for opcode in opcodes {
            let parameter_count = match opcode {
                0x00 | 0x41 => 0,
                0x14 | 0x20 | 0x21 | 0x22 | 0x24..=0x28 | 0x40 => 1,
                0x1B => 3,
                0x23 => 2,
                _ => return Err(format!("unsupported Nitro GPU opcode 0x{opcode:02X}")),
            };
            let mut parameters = [0u32; 3];
            for parameter in parameters.iter_mut().take(parameter_count) {
                *parameter = read_gpu_word(bytes, &mut cursor)?;
            }
            match opcode {
                0x20 => current_color = rgb555(parameters[0]),
                0x40 => {
                    primitive_mode = Some(parameters[0] as u8 & 3);
                    primitive_vertices.clear();
                }
                0x41 => {
                    if let Some(mode) = primitive_mode.take() {
                        triangles.extend(triangulate(&primitive_vertices, mode));
                    }
                    primitive_vertices.clear();
                }
                0x23 => {
                    let xy = parameters[0];
                    let z = parameters[1];
                    current = [
                        i16::from_le_bytes((xy as u16).to_le_bytes()) as f64 / 4096.0,
                        i16::from_le_bytes(((xy >> 16) as u16).to_le_bytes()) as f64 / 4096.0,
                        i16::from_le_bytes((z as u16).to_le_bytes()) as f64 / 4096.0,
                    ];
                    push_vertex(
                        &mut primitive_vertices,
                        primitive_mode,
                        current,
                        current_color,
                    );
                }
                0x24 => {
                    let packed = parameters[0];
                    current = [
                        signed_bits(packed, 0, 10) as f64 / 64.0,
                        signed_bits(packed, 10, 10) as f64 / 64.0,
                        signed_bits(packed, 20, 10) as f64 / 64.0,
                    ];
                    push_vertex(
                        &mut primitive_vertices,
                        primitive_mode,
                        current,
                        current_color,
                    );
                }
                0x25 => {
                    current[0] =
                        i16::from_le_bytes((parameters[0] as u16).to_le_bytes()) as f64 / 4096.0;
                    current[1] = i16::from_le_bytes(((parameters[0] >> 16) as u16).to_le_bytes())
                        as f64
                        / 4096.0;
                    push_vertex(
                        &mut primitive_vertices,
                        primitive_mode,
                        current,
                        current_color,
                    );
                }
                0x26 => {
                    current[0] =
                        i16::from_le_bytes((parameters[0] as u16).to_le_bytes()) as f64 / 4096.0;
                    current[2] = i16::from_le_bytes(((parameters[0] >> 16) as u16).to_le_bytes())
                        as f64
                        / 4096.0;
                    push_vertex(
                        &mut primitive_vertices,
                        primitive_mode,
                        current,
                        current_color,
                    );
                }
                0x27 => {
                    current[1] =
                        i16::from_le_bytes((parameters[0] as u16).to_le_bytes()) as f64 / 4096.0;
                    current[2] = i16::from_le_bytes(((parameters[0] >> 16) as u16).to_le_bytes())
                        as f64
                        / 4096.0;
                    push_vertex(
                        &mut primitive_vertices,
                        primitive_mode,
                        current,
                        current_color,
                    );
                }
                0x28 => {
                    let packed = parameters[0];
                    current[0] += signed_bits(packed, 0, 10) as f64 / 4096.0;
                    current[1] += signed_bits(packed, 10, 10) as f64 / 4096.0;
                    current[2] += signed_bits(packed, 20, 10) as f64 / 4096.0;
                    push_vertex(
                        &mut primitive_vertices,
                        primitive_mode,
                        current,
                        current_color,
                    );
                }
                // Matrix, texture-coordinate and normal commands do not alter
                // the positions used by this flat-color preview.
                _ => {}
            }
        }
    }
    Ok(triangles)
}

fn push_vertex(vertices: &mut Vec<Vertex>, mode: Option<u8>, position: [f64; 3], color: [u8; 3]) {
    if mode.is_some() {
        vertices.push(Vertex { position, color });
    }
}

fn triangulate(vertices: &[Vertex], mode: u8) -> Vec<Triangle> {
    let mut output = Vec::new();
    match mode {
        0 => {
            for triangle in vertices.chunks_exact(3) {
                output.push(make_triangle([triangle[0], triangle[1], triangle[2]], None));
            }
        }
        1 => {
            for quad in vertices.chunks_exact(4) {
                output.push(make_triangle([quad[0], quad[1], quad[2]], None));
                output.push(make_triangle([quad[0], quad[2], quad[3]], None));
            }
        }
        2 => {
            for index in 2..vertices.len() {
                let triangle = if index % 2 == 0 {
                    [vertices[index - 2], vertices[index - 1], vertices[index]]
                } else {
                    [vertices[index - 1], vertices[index - 2], vertices[index]]
                };
                output.push(make_triangle(triangle, None));
            }
        }
        3 => {
            for index in (0..vertices.len().saturating_sub(2)).step_by(2) {
                if index + 3 < vertices.len() {
                    output.push(make_triangle(
                        [vertices[index], vertices[index + 1], vertices[index + 3]],
                        None,
                    ));
                    output.push(make_triangle(
                        [vertices[index], vertices[index + 3], vertices[index + 2]],
                        None,
                    ));
                }
            }
        }
        _ => {}
    }
    output
}

fn make_triangle(vertices: [Vertex; 3], material_color: Option<[u8; 3]>) -> Triangle {
    Triangle {
        vertices,
        material_color,
        depth: 0.0,
    }
}

pub(super) fn write_svg(zone: &ZoneMap, triangles: &[Triangle]) -> Result<String> {
    let mut projected = triangles
        .iter()
        .map(|triangle| {
            let points = triangle.vertices.map(|vertex| {
                let [x, y, z] = vertex.position;
                [x - z, (x + z) * 0.5 - y]
            });
            let color = triangle.material_color.unwrap_or_else(|| {
                let mut color = [0u8; 3];
                for vertex in triangle.vertices {
                    for (channel, value) in color.iter_mut().zip(vertex.color) {
                        *channel = channel.saturating_add(value / 3);
                    }
                }
                color
            });
            (triangle.depth, points, color)
        })
        .collect::<Vec<_>>();
    let mut bounds = [
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    ];
    for (_, points, _) in &projected {
        for [x, y] in points {
            bounds[0] = bounds[0].min(*x);
            bounds[1] = bounds[1].min(*y);
            bounds[2] = bounds[2].max(*x);
            bounds[3] = bounds[3].max(*y);
        }
    }
    let width = (bounds[2] - bounds[0]).max(1.0);
    let height = (bounds[3] - bounds[1]).max(1.0);
    let scale = (1120.0 / width).min(690.0 / height);
    projected.sort_by(|left, right| left.0.total_cmp(&right.0));

    let mut svg = String::with_capacity(projected.len() * 96 + 512);
    svg.push_str("<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 1280 900\" width=\"1280\" height=\"900\">\n");
    svg.push_str("<rect width=\"1280\" height=\"900\" fill=\"#eef2f6\"/>\n");
    svg.push_str("<text x=\"32\" y=\"42\" font-family=\"sans-serif\" font-size=\"25\" font-weight=\"700\" fill=\"#172033\">");
    svg.push_str(zone.display_name());
    svg.push_str(" — BW2 map geometry</text>\n");
    let model_ids = zone
        .model_ids
        .iter()
        .flatten()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    svg.push_str(&format!(
        "<text x=\"32\" y=\"70\" font-family=\"sans-serif\" font-size=\"15\" fill=\"#40506a\">zone {} · area {} · matrix {} ({}×{}) · model {} · {} triangles</text>\n",
        zone.zone_id,
        zone.area_id,
        zone.matrix_id,
        zone.width,
        zone.height,
        model_ids,
        triangles.len()
    ));
    svg.push_str("<g stroke=\"#273246\" stroke-width=\"0.35\" stroke-linejoin=\"round\">\n");
    for (_, points, color) in projected {
        let points = points
            .iter()
            .map(|[x, y]| {
                let sx = 640.0 + (*x - (bounds[0] + bounds[2]) * 0.5) * scale;
                let sy = 450.0 - (*y - (bounds[1] + bounds[3]) * 0.5) * scale;
                format!("{sx:.2},{sy:.2}")
            })
            .collect::<Vec<_>>()
            .join(" ");
        svg.push_str(&format!(
            "<polygon points=\"{points}\" fill=\"#{:02x}{:02x}{:02x}\"/>\n",
            color[0], color[1], color[2]
        ));
    }
    svg.push_str("</g>\n</svg>\n");
    Ok(svg)
}

fn read_gpu_word(bytes: &[u8], cursor: &mut usize) -> Result<u32> {
    let value = u32_at(bytes, *cursor)?;
    *cursor += 4;
    Ok(value)
}

fn signed_bits(value: u32, start: u32, width: u32) -> i32 {
    let mask = (1u32 << width) - 1;
    let value = (value >> start) & mask;
    if value & (1 << (width - 1)) != 0 {
        (value | !mask) as i32
    } else {
        value as i32
    }
}

fn rgb555(value: u32) -> [u8; 3] {
    let expand = |shift| (((value >> shift) & 31u32) * 255u32 / 31u32) as u8;
    [expand(0), expand(5), expand(10)]
}

fn require(bytes: &[u8], start: usize, length: usize, what: &str) -> Result<()> {
    let end = start
        .checked_add(length)
        .ok_or_else(|| format!("{what} range overflowed"))?;
    if end > bytes.len() {
        return Err(format!("{what} extends past its containing file"));
    }
    Ok(())
}

fn u16_at(bytes: &[u8], offset: usize) -> Result<u16> {
    require(bytes, offset, 2, "16-bit field")?;
    Ok(u16::from_le_bytes([bytes[offset], bytes[offset + 1]]))
}

fn i16_at(bytes: &[u8], offset: usize) -> Result<i16> {
    require(bytes, offset, 2, "16-bit field")?;
    Ok(i16::from_le_bytes([bytes[offset], bytes[offset + 1]]))
}

fn u32_at(bytes: &[u8], offset: usize) -> Result<u32> {
    require(bytes, offset, 4, "32-bit field")?;
    Ok(u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ]))
}

fn i32_at(bytes: &[u8], offset: usize) -> Result<i32> {
    Ok(u32_at(bytes, offset)? as i32)
}

fn usize_at_u32(bytes: &[u8], offset: usize) -> Result<usize> {
    usize::try_from(u32_at(bytes, offset)?)
        .map_err(|_| "32-bit offset does not fit this platform".to_owned())
}
