use core::fmt;

use byteorder::{ByteOrder, NetworkEndian};
use core_io::{Error as IoError, Read, Write};
use crc::crc32::checksum_ieee;
use io::Cursor;

pub const CTRL_PACKET_MAXSIZE: usize = 128; // for compatibility with version1.x compliant Devices - Section 12.1.6 (CXP-001-2021)
pub const DATA_MAXSIZE: usize =
    CTRL_PACKET_MAXSIZE - /*packet start KCodes, data packet types, CMD, Tag, Addr, CRC, packet end KCode*/4*7;

pub enum Error {
    CorruptedPacket,
    CtrlAckError(u8),
    Io(IoError),
    LengthOutOfRange(u32),
    TagMismatch,
    TimedOut,
    UnexpectedReply,
    UnknownPacket(u8),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            &Error::CorruptedPacket => write!(f, "CorruptedPacket - Received packet fail CRC test"),
            &Error::CtrlAckError(ref ack_code) => match ack_code {
                0x40 => write!(f, "CtrlAckError - Invalid Address"),
                0x41 => write!(f, "CtrlAckError - Invalid data for the address"),
                0x42 => write!(f, "CtrlAckError - Invalid operation code"),
                0x43 => write!(f, "CtrlAckError - Write attempted to a read-only address"),
                0x44 => write!(f, "CtrlAckError - Read attempted from a write-only address"),
                0x45 => write!(f, "CtrlAckError - Size field too large, exceed packet size limit"),
                0x46 => write!(f, "CtrlAckError - Message size is inconsistent with size field"),
                0x47 => write!(f, "CtrlAckError - Malformed packet"),
                0x80 => write!(f, "CtrlAckError - Failed CRC test in last received command"),
                _ => write!(f, "CtrlAckError - Unknown ack code {:#X}", ack_code),
            },
            &Error::Io(ref err) => write!(f, "IoError - {:?}", err),
            &Error::LengthOutOfRange(length) => write!(
                f,
                "LengthOutOfRange - Message length {} is not between 1 and {}",
                length, DATA_MAXSIZE
            ),
            &Error::TagMismatch => write!(f, "TagMismatch - Received tag is different from the transmitted tag"),
            &Error::TimedOut => write!(f, "MessageTimedOut"),
            &Error::UnexpectedReply => write!(f, "UnexpectedReply"),
            &Error::UnknownPacket(packet_type) => {
                write!(f, "UnknownPacket - Unknown packet type id {:#X} ", packet_type)
            }
        }
    }
}

impl From<IoError> for Error {
    fn from(value: IoError) -> Error {
        Error::Io(value)
    }
}

fn get_cxp_crc(bytes: &[u8]) -> u32 {
    // Section 9.2.2.2 (CXP-001-2021)
    // Only Control packet need CRC32 appended in the end of the packet
    // CoaXpress use the polynomial of IEEE-802.3 (Ethernet) CRC but the checksum calculation is different
    (!checksum_ieee(bytes)).swap_bytes()
}

trait CxpRead {
    fn read_u8(&mut self) -> Result<u8, Error>;

    fn read_u16(&mut self) -> Result<u16, Error>;

    fn read_u32(&mut self) -> Result<u32, Error>;

    fn read_u64(&mut self) -> Result<u64, Error>;

    fn read_exact_4x(&mut self, buf: &mut [u8]) -> Result<(), Error>;

    fn read_4x_u8(&mut self) -> Result<u8, Error>;

    fn read_4x_u16(&mut self) -> Result<u16, Error>;

    fn read_4x_u32(&mut self) -> Result<u32, Error>;
}
impl<Cursor: Read> CxpRead for Cursor {
    fn read_u8(&mut self) -> Result<u8, Error> {
        let mut bytes = [0; 1];
        self.read_exact(&mut bytes)?;
        Ok(bytes[0])
    }

    fn read_u16(&mut self) -> Result<u16, Error> {
        let mut bytes = [0; 2];
        self.read_exact(&mut bytes)?;
        Ok(NetworkEndian::read_u16(&bytes))
    }

    fn read_u32(&mut self) -> Result<u32, Error> {
        let mut bytes = [0; 4];
        self.read_exact(&mut bytes)?;
        Ok(NetworkEndian::read_u32(&bytes))
    }

    fn read_u64(&mut self) -> Result<u64, Error> {
        let mut bytes = [0; 8];
        self.read_exact(&mut bytes)?;
        Ok(NetworkEndian::read_u64(&bytes))
    }

    fn read_exact_4x(&mut self, buf: &mut [u8]) -> Result<(), Error> {
        for byte in buf {
            // Section 9.2.2.1 (CXP-001-2021)
            // decoder should immune to single bit errors when handling 4x duplicated characters
            let a = self.read_u8()?;
            let b = self.read_u8()?;
            let c = self.read_u8()?;
            let d = self.read_u8()?;
            // vote and return majority
            *byte = a & b & c | a & b & d | a & c & d | b & c & d;
        }
        Ok(())
    }

    fn read_4x_u8(&mut self) -> Result<u8, Error> {
        let mut bytes = [0; 1];
        self.read_exact_4x(&mut bytes)?;
        Ok(bytes[0])
    }

    fn read_4x_u16(&mut self) -> Result<u16, Error> {
        let mut bytes = [0; 2];
        self.read_exact_4x(&mut bytes)?;
        Ok(NetworkEndian::read_u16(&bytes))
    }

    fn read_4x_u32(&mut self) -> Result<u32, Error> {
        let mut bytes = [0; 4];
        self.read_exact_4x(&mut bytes)?;
        Ok(NetworkEndian::read_u32(&bytes))
    }
}

#[derive(Debug)]
pub enum RXCTRLPacket {
    CtrlReply {
        tag: Option<u8>,
        length: u32,
        data: [u8; DATA_MAXSIZE],
    },
    CtrlDelay {
        tag: Option<u8>,
        time: u32,
    },
    CtrlAck {
        tag: Option<u8>,
    },
}

impl RXCTRLPacket {
    pub fn read_from(reader: &mut Cursor<&mut [u8]>) -> Result<Self, Error> {
        match reader.read_4x_u8()? {
            0x03 => RXCTRLPacket::get_ctrl_packet(reader, false),
            0x06 => RXCTRLPacket::get_ctrl_packet(reader, true),
            ty => Err(Error::UnknownPacket(ty)),
        }
    }

    fn get_ctrl_packet(reader: &mut Cursor<&mut [u8]>, with_tag: bool) -> Result<Self, Error> {
        let mut tag: Option<u8> = None;
        if with_tag {
            tag = Some(reader.read_4x_u8()?);
        }

        let ackcode = reader.read_4x_u8()?;

        match ackcode {
            0x00 | 0x04 => {
                let length = reader.read_u32()?;
                let mut data: [u8; DATA_MAXSIZE] = [0; DATA_MAXSIZE];
                reader.read(&mut data[0..length as usize])?;

                // Section 9.6.3 (CXP-001-2021)
                // when length is not multiple of 4, dummy bits are padded to align to the word boundary
                // set position to next word boundary for CRC calculation
                let padding = (4 - (reader.position() % 4)) % 4;
                reader.set_position(reader.position() + padding);

                // Section 9.6.3 (CXP-001-2021)
                // only bytes after the first 4 are used in calculating the checksum
                let checksum = get_cxp_crc(&reader.get_ref()[4..reader.position()]);
                if reader.read_u32()? != checksum {
                    return Err(Error::CorruptedPacket);
                }

                if ackcode == 0x00 {
                    return Ok(RXCTRLPacket::CtrlReply { tag, length, data });
                } else {
                    return Ok(RXCTRLPacket::CtrlDelay {
                        tag,
                        time: NetworkEndian::read_u32(&data[..4]),
                    });
                }
            }
            0x01 => return Ok(RXCTRLPacket::CtrlAck { tag }),
            _ => return Err(Error::CtrlAckError(ackcode)),
        }
    }
}

trait CxpWrite {
    fn write_u8(&mut self, value: u8) -> Result<(), Error>;

    fn write_u32(&mut self, value: u32) -> Result<(), Error>;

    fn write_all_4x(&mut self, buf: &[u8]) -> Result<(), Error>;

    fn write_4x_u8(&mut self, value: u8) -> Result<(), Error>;

    fn write_4x_u16(&mut self, value: u16) -> Result<(), Error>;

    fn write_4x_u32(&mut self, value: u32) -> Result<(), Error>;
}
impl<Cursor: Write> CxpWrite for Cursor {
    fn write_u8(&mut self, value: u8) -> Result<(), Error> {
        self.write_all(&[value])?;
        Ok(())
    }

    fn write_u32(&mut self, value: u32) -> Result<(), Error> {
        let mut bytes = [0; 4];
        NetworkEndian::write_u32(&mut bytes, value);
        self.write_all(&bytes)?;
        Ok(())
    }

    fn write_all_4x(&mut self, buf: &[u8]) -> Result<(), Error> {
        for byte in buf {
            self.write_all(&[*byte; 4])?;
        }
        Ok(())
    }

    fn write_4x_u8(&mut self, value: u8) -> Result<(), Error> {
        self.write_all_4x(&[value])
    }

    fn write_4x_u16(&mut self, value: u16) -> Result<(), Error> {
        let mut bytes = [0; 2];
        NetworkEndian::write_u16(&mut bytes, value);
        self.write_all_4x(&bytes)
    }

    fn write_4x_u32(&mut self, value: u32) -> Result<(), Error> {
        let mut bytes = [0; 4];
        NetworkEndian::write_u32(&mut bytes, value);
        self.write_all_4x(&bytes)
    }
}

#[derive(Debug)]
pub enum TXCTRLPacket {
    CtrlRead {
        tag: Option<u8>,
        addr: u32,
        length: u32,
    },
    CtrlWrite {
        tag: Option<u8>,
        addr: u32,
        length: u32,
        data: [u8; DATA_MAXSIZE],
    },
}

impl TXCTRLPacket {
    pub fn write_to(&self, writer: &mut Cursor<&mut [u8]>) -> Result<(), Error> {
        match *self {
            TXCTRLPacket::CtrlRead { tag, addr, length } => {
                match tag {
                    Some(t) => {
                        writer.write_4x_u8(0x05)?;
                        writer.write_4x_u8(t)?;
                    }
                    None => {
                        writer.write_4x_u8(0x02)?;
                    }
                }

                let mut bytes = [0; 3];
                NetworkEndian::write_u24(&mut bytes, length);
                writer.write_all(&[0x00, bytes[0], bytes[1], bytes[2]])?;

                writer.write_u32(addr)?;

                // Section 9.6.2 (CXP-001-2021)
                // only bytes after the first 4 are used in calculating the checksum
                let checksum = get_cxp_crc(&writer.get_ref()[4..writer.position()]);
                writer.write_u32(checksum)?;
            }
            TXCTRLPacket::CtrlWrite {
                tag,
                addr,
                length,
                data,
            } => {
                match tag {
                    Some(t) => {
                        writer.write_4x_u8(0x05)?;
                        writer.write_4x_u8(t)?;
                    }
                    None => {
                        writer.write_4x_u8(0x02)?;
                    }
                }

                let mut bytes = [0; 3];
                NetworkEndian::write_u24(&mut bytes, length);
                writer.write_all(&[0x01, bytes[0], bytes[1], bytes[2]])?;

                writer.write_u32(addr)?;
                writer.write_all(&data[0..length as usize])?;

                // Section 9.6.2 (CXP-001-2021)
                // when length is not multiple of 4, dummy bites are padded to align to the word boundary
                let padding = (4 - (writer.position() % 4)) % 4;
                for _ in 0..padding {
                    writer.write_u8(0)?;
                }

                // Section 9.6.2 (CXP-001-2021)
                // only bytes after the first 4 are used in calculating the checksum
                let checksum = get_cxp_crc(&writer.get_ref()[4..writer.position()]);
                writer.write_u32(checksum)?;
            }
        }
        Ok(())
    }
}
