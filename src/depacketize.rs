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
