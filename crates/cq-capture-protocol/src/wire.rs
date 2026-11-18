use crate::Packet;
use std::io::{self, Read, Write};

pub fn read_bytes(reader: &mut impl Read, max: usize) -> io::Result<Option<Vec<u8>>> {
    let mut head = [0; 4];
    loop {
        match reader.read(&mut head[..1]) {
            Ok(0) => return Ok(None),
            Ok(_) => break,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    reader.read_exact(&mut head[1..])?;
    let len = u32::from_be_bytes(head) as usize;
    if len == 0 || len > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame exceeds budget",
        ));
    }
    let mut bytes = vec![0; len];
    reader.read_exact(&mut bytes)?;
    Ok(Some(bytes))
}
pub fn decode(bytes: &[u8]) -> io::Result<Packet> {
    serde_json::from_slice(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}
pub fn write_bytes(writer: &mut impl Write, bytes: &[u8], max: usize) -> io::Result<()> {
    if bytes.is_empty() || bytes.len() > max || bytes.len() > u32::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame exceeds budget",
        ));
    }
    writer.write_all(&(bytes.len() as u32).to_be_bytes())?;
    writer.write_all(bytes)?;
    writer.flush()
}
pub fn write_packet(writer: &mut impl Write, packet: &Packet, max: usize) -> io::Result<()> {
    let bytes = serde_json::to_vec(packet).map_err(io::Error::other)?;
    write_bytes(writer, &bytes, max)
}
