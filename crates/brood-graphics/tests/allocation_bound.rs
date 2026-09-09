use std::mem::size_of;

use brood_graphics::{Color, Command, MAX_SAVED_STATES, Packet, Path, Rect};

fn header(command_count: u32, declared_bytes: u32) -> Vec<u8> {
    let mut bytes = Vec::from(*b"MFGX");
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&0_u16.to_le_bytes());
    bytes.extend_from_slice(&command_count.to_le_bytes());
    bytes.extend_from_slice(&declared_bytes.to_le_bytes());
    bytes
}

#[test]
fn valid_packet_bound_covers_decoded_storage_and_validation_scratch()
-> Result<(), brood_graphics::PacketError> {
    let mut packet = Packet::new();
    packet.push(Command::Save);
    packet.push(Command::ClipPath(
        Path::new().move_to(1.0, 2.0).line_to(3.0, 4.0),
    ));
    packet.push(Command::Restore);
    packet.push(Command::FillRect {
        rect: Rect::new(0.0, 0.0, 10.0, 10.0),
        color: Color::rgba(1.0, 0.0, 0.0, 1.0),
    });
    let encoded = packet.encode()?;
    let bound = Packet::decode_allocation_bound(&encoded)?;
    let decoded = Packet::decode(&encoded)?;
    let command_storage = decoded.commands.capacity() * size_of::<Command>();
    let path_storage = 2 * size_of::<brood_graphics::PathSegment>();
    let validation_scratch = MAX_SAVED_STATES * size_of::<usize>();
    if bound < command_storage + path_storage + validation_scratch {
        return Err(brood_graphics::PacketError::Invalid(
            "allocation bound omitted decoded storage or validation scratch",
        ));
    }
    Ok(())
}

#[test]
fn huge_command_count_is_rejected_before_list_allocation() {
    let bytes = header(0xffff_ffff, 16);
    assert!(Packet::decode_allocation_bound(&bytes).is_err());
    assert!(Packet::decode(&bytes).is_err());
}

#[test]
fn huge_path_count_is_rejected_before_segment_allocation() {
    let mut bytes = header(1, 28);
    bytes.extend_from_slice(&[7, 0, 0, 0]);
    bytes.extend_from_slice(&4_u32.to_le_bytes());
    bytes.extend_from_slice(&0xffff_ffff_u32.to_le_bytes());
    assert!(Packet::decode_allocation_bound(&bytes).is_err());
    assert!(Packet::decode(&bytes).is_err());
}
