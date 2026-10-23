use anyhow::{Result, ensure};

pub const FRAME_START_BINA_REL: usize = 0x00fa6e3c;
pub const FRAME_HEADER_SIZE: usize = 0x1c;
pub const FRAME_PAYLOAD_SIZE: usize = 0x200;
pub const FRAME_SIZE: usize = FRAME_HEADER_SIZE + FRAME_PAYLOAD_SIZE;

pub fn depacketize_bina(bina: &[u8]) -> Vec<u8> {
    if bina.len() <= FRAME_START_BINA_REL {
        return bina.to_vec();
    }

    let mut out = bina[..FRAME_START_BINA_REL].to_vec();
    let mut pos = FRAME_START_BINA_REL;

    while pos + FRAME_SIZE <= bina.len() {
        if bina[pos + 2] != 0x00 || bina[pos + 3] != 0x04 {
            break;
        }

        out.extend_from_slice(&bina[pos + FRAME_HEADER_SIZE..pos + FRAME_SIZE]);
        pos += FRAME_SIZE;
    }

    out.extend_from_slice(&bina[pos..]);
    out
}

pub fn clean_bina_rel_to_original_abs(clean_rel: usize, bina_abs_offset: usize) -> usize {
    if clean_rel < FRAME_START_BINA_REL {
        return bina_abs_offset + clean_rel;
    }

    let delta = clean_rel - FRAME_START_BINA_REL;
    let q = delta / FRAME_PAYLOAD_SIZE;
    let r = delta % FRAME_PAYLOAD_SIZE;

    bina_abs_offset + FRAME_START_BINA_REL + q * FRAME_SIZE + FRAME_HEADER_SIZE + r
}

/// How many clean bytes starting at `clean_rel` are contiguous in the original BINA layout.
fn contiguous_run(clean_rel: usize) -> usize {
    if clean_rel < FRAME_START_BINA_REL {
        FRAME_START_BINA_REL - clean_rel
    } else {
        let remainder = (clean_rel - FRAME_START_BINA_REL) % FRAME_PAYLOAD_SIZE;
        FRAME_PAYLOAD_SIZE - remainder
    }
}

/// Writes `bytes` at `clean_rel` in the depacketized BINA, mapping each byte back through the
/// frame wrapper so frame headers stay intact.
pub fn patch_clean_range(
    firmware: &mut [u8],
    bina_abs_offset: usize,
    mut clean_rel: usize,
    bytes: &[u8],
) -> Result<()> {
    let mut off = 0;
    while off < bytes.len() {
        let abs = clean_bina_rel_to_original_abs(clean_rel, bina_abs_offset);
        let n = contiguous_run(clean_rel).min(bytes.len() - off);
        let end = abs + n;
        ensure!(
            end <= firmware.len(),
            "patch offset out of bounds at clean_rel=0x{clean_rel:x}, abs=0x{abs:x} ({n} bytes)"
        );
        firmware[abs..end].copy_from_slice(&bytes[off..off + n]);
        off += n;
        clean_rel += n;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_clean_range_round_trips_across_frames() {
        let mut payload = vec![0u8; FRAME_PAYLOAD_SIZE * 3];
        for (i, byte) in payload.iter_mut().enumerate() {
            *byte = (i % 251) as u8;
        }

        let mut bina = vec![0x5Au8; FRAME_START_BINA_REL];
        for quarter in 0..3 {
            let mut header = vec![0xABu8; FRAME_HEADER_SIZE];
            header[2] = 0x00;
            header[3] = 0x04;
            bina.extend_from_slice(&header);
            let start = quarter * FRAME_PAYLOAD_SIZE;
            bina.extend_from_slice(&payload[start..start + FRAME_PAYLOAD_SIZE]);
        }

        let clean = depacketize_bina(&bina);
        assert_eq!(&clean[FRAME_START_BINA_REL..], payload.as_slice());

        let start = FRAME_START_BINA_REL + FRAME_PAYLOAD_SIZE - 10;
        let patch = vec![0xEEu8; 30];
        patch_clean_range(&mut bina, 0, start, &patch).unwrap();

        for quarter in 0..3 {
            let header = FRAME_START_BINA_REL + quarter * FRAME_SIZE;
            assert_eq!(bina[header], 0xAB, "frame header was overwritten");
            assert_eq!(bina[header + 2], 0x00);
            assert_eq!(bina[header + 3], 0x04);
        }

        let patched = depacketize_bina(&bina);
        assert_eq!(&patched[start..start + patch.len()], patch.as_slice());
        assert_eq!(patched[start - 1], clean[start - 1]);
        assert_eq!(patched[start + patch.len()], clean[start + patch.len()]);
        // The head, before the frame wrapper, is addressable too.
        patch_clean_range(&mut bina, 0, 4, &[1, 2, 3, 4]).unwrap();
        assert_eq!(&depacketize_bina(&bina)[4..8], &[1, 2, 3, 4]);
    }
}
