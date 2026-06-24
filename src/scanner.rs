use crate::image_format::{ImageRecord, validate_image_record};

pub const IMAGE_SCAN_START_CANDIDATE_BINA_REL: usize = 0x0141f4a0;

pub fn find_image_records(
    clean_bina: &[u8],
    scan_start: usize,
    min_area: usize,
) -> Vec<ImageRecord> {
    let mut records = Vec::new();
    let mut off = scan_start;

    while off + crate::image_format::IMAGE_HEADER_SIZE <= clean_bina.len() {
        if let Some(record) = validate_image_record(clean_bina, off) {
            if record.area() >= min_area {
                records.push(record);
            }
        }
        off += 4;
    }

    remove_overlaps(records)
}

pub fn remove_overlaps(mut records: Vec<ImageRecord>) -> Vec<ImageRecord> {
    records.sort_by_key(|r| r.offset);

    let mut selected = Vec::new();
    let mut current_end = 0usize;

    for record in records {
        if record.offset >= current_end {
            current_end = record.end();
            selected.push(record);
        }
    }

    selected
}
