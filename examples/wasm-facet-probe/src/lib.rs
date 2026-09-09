wit_bindgen::generate!({
    path: "wit",
    world: "facet-probe",
});

struct Facet;

impl Guest for Facet {
    fn render(input: Vec<u8>) -> Vec<u8> {
        if input == b"oversized-output-probe" {
            // The host's pre-instantiation memory limiter must trap this before
            // canonical lifting can allocate an unbounded host-side list.
            return vec![0; 5 * 1024 * 1024];
        }
        let checksum = input
            .iter()
            .fold(0_u64, |sum, byte| sum.wrapping_add(u64::from(*byte)));
        let mut output = Vec::with_capacity(16);
        output.extend_from_slice(&checksum.to_le_bytes());
        output.extend_from_slice(&u64::try_from(input.len()).unwrap_or(u64::MAX).to_le_bytes());
        output
    }
}

export!(Facet);
