//! Nintendo "backward LZ" (BLZ) decompression, used for the ARM9 static
//! binary and ARM9 overlays.
//!
//! A BLZ image keeps an uncompressed prefix and compresses the tail, decoding
//! from the end of the buffer towards the start. An 8-byte footer describes
//! the compressed region:
//!
//! - `u32` at `len - 8`: bits 0-23 are the compressed region length (footer
//!   included), bits 24-31 are the footer length.
//! - `u32` at `len - 4`: how many bytes decompression adds.

use crate::Result;

pub fn decompress(input: &[u8]) -> Result<Vec<u8>> {
    if input.len() < 8 {
        return Err("BLZ image is shorter than its 8-byte footer".to_owned());
    }
    let len = input.len();
    let packed = u32::from_le_bytes(input[len - 8..len - 4].try_into().unwrap_or_default());
    let extra = u32::from_le_bytes(input[len - 4..].try_into().unwrap_or_default()) as usize;
    let enc_len = (packed & 0x00FF_FFFF) as usize;
    let hdr_len = (packed >> 24) as usize;

    if extra == 0 {
        // Not compressed: some tools mark this with an empty footer.
        return Ok(input.to_vec());
    }
    if !(8..=0x0B).contains(&hdr_len) {
        return Err(format!("BLZ footer length {hdr_len} is invalid"));
    }
    if enc_len < hdr_len || enc_len > len {
        return Err(format!(
            "BLZ compressed length 0x{enc_len:X} is invalid for a 0x{len:X}-byte image"
        ));
    }

    let raw_len = len
        .checked_add(extra)
        .ok_or_else(|| "BLZ decompressed size overflowed".to_owned())?;
    let prefix_len = len - enc_len;
    let mut output = vec![0_u8; raw_len];
    output[..prefix_len].copy_from_slice(&input[..prefix_len]);

    let mut src = len - hdr_len;
    let mut dst = raw_len;
    let mut flags = 0_u8;
    let mut mask = 0_u8;
    while dst > prefix_len {
        if mask == 0 {
            src = src
                .checked_sub(1)
                .filter(|&value| value >= prefix_len)
                .ok_or_else(|| "BLZ stream ended while reading flags".to_owned())?;
            flags = input[src];
            mask = 0x80;
        }
        if flags & mask == 0 {
            src = src
                .checked_sub(1)
                .filter(|&value| value >= prefix_len)
                .ok_or_else(|| "BLZ stream ended while reading a literal".to_owned())?;
            dst -= 1;
            output[dst] = input[src];
        } else {
            if src < prefix_len + 2 {
                return Err("BLZ stream ended while reading a back-reference".to_owned());
            }
            let pair = (u16::from(input[src - 1]) << 8) | u16::from(input[src - 2]);
            src -= 2;
            let count = usize::from(pair >> 12) + 3;
            let distance = usize::from(pair & 0x0FFF) + 3;
            if count > dst - prefix_len {
                return Err(format!(
                    "BLZ back-reference of {count} bytes overruns the uncompressed prefix"
                ));
            }
            for _ in 0..count {
                dst -= 1;
                let from = dst + distance;
                if from >= raw_len {
                    return Err(format!(
                        "BLZ back-reference distance {distance} points past the image end"
                    ));
                }
                output[dst] = output[from];
            }
        }
        mask >>= 1;
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::decompress;

    /// Append a standard 8-byte BLZ footer to a prefix and an encoded body.
    fn image(prefix: &[u8], body: &[u8], raw_len: usize) -> Vec<u8> {
        let mut bytes = prefix.to_vec();
        bytes.extend_from_slice(body);
        let enc_len = (body.len() + 8) as u32;
        bytes.extend_from_slice(&(enc_len | (8 << 24)).to_le_bytes());
        let extra = (raw_len - (bytes.len() + 4)) as u32;
        bytes.extend_from_slice(&extra.to_le_bytes());
        bytes
    }

    #[test]
    fn decodes_literals_and_back_references() {
        // The stream is read from the end: flags, then literals c, b, a, then
        // two 18-byte references at distance 3, each written further back.
        let reference = ((18_u16 - 3) << 12).to_le_bytes();
        let body = [
            reference[0],
            reference[1],
            reference[0],
            reference[1],
            b'a',
            b'b',
            b'c',
            0b0001_1000,
        ];
        let mut wanted = b"P".to_vec();
        wanted.extend(b"abc".repeat(13));
        let bytes = image(b"P", &body, wanted.len());
        assert_eq!(decompress(&bytes).unwrap(), wanted);
    }

    #[test]
    fn rejects_reference_past_prefix() {
        let reference = ((18_u16 - 3) << 12).to_le_bytes();
        let body = [reference[0], reference[1], b'c', 0b0100_0000];
        assert!(decompress(&image(b"P", &body, 15)).is_err());
    }

    #[test]
    fn rejects_short_input() {
        assert!(decompress(&[0; 4]).is_err());
    }

    #[test]
    fn passes_through_uncompressed_footer() {
        let bytes = [1, 2, 3, 4, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(decompress(&bytes).unwrap(), bytes);
    }
}
