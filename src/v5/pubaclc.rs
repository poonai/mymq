use crate::util::advance;
use crate::v5::{FixedHeader, PacketType, Property, PropertyType};
use crate::{Blob, Packetize, UserProperty, VarU32};
use crate::{Error, ErrorKind, ReasonCode, Result};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ReasCode {
    Success = 0x00,
    NoMatchingSubscribers = 0x10,
    UnspecifiedError = 0x80,
    ImplementationSpecificError = 0x83,
    NotAuthorized = 0x87,
    TopicNameInvalid = 0x90,
    PacketIdentifierInUse = 0x91,
    PacketIdNotFound = 0x92,
    QuotaExceeded = 0x97,
    PayloadFormatInvalid = 0x99,
}

impl TryFrom<u8> for ReasCode {
    type Error = Error;

    fn try_from(val: u8) -> Result<ReasCode> {
        match val {
            0x00 => Ok(ReasCode::Success),
            0x10 => Ok(ReasCode::NoMatchingSubscribers),
            0x80 => Ok(ReasCode::UnspecifiedError),
            0x83 => Ok(ReasCode::ImplementationSpecificError),
            0x87 => Ok(ReasCode::NotAuthorized),
            0x90 => Ok(ReasCode::TopicNameInvalid),
            0x91 => Ok(ReasCode::PacketIdentifierInUse),
            0x92 => Ok(ReasCode::PacketIdNotFound),
            0x97 => Ok(ReasCode::QuotaExceeded),
            0x99 => Ok(ReasCode::PayloadFormatInvalid),
            val => err!(ProtocolError, code: ProtocolError, "reason-code {:?}", val),
        }
    }
}

impl Default for ReasCode {
    fn default() -> ReasCode {
        ReasCode::Success
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PubARLC {
    pub packet_type: PacketType,
    pub packet_id: u16,
    pub code: ReasCode,
    pub properties: Option<Properties>,
}

impl Packetize for PubARLC {
    fn decode(stream: &[u8]) -> Result<(Self, usize)> {
        use crate::dec_props;

        let code: ReasCode = ReasCode::Success;
        let properties: Option<Properties> = None;

        let (fh, mut n) = FixedHeader::decode(stream)?;
        fh.validate()?;
        let (packet_type, _, _, _) = fh.unwrap()?;

        let (packet_id, m) = u16::decode(advance(stream, n)?)?;
        n += m;

        if *fh.remaining_len == 2 {
            let packet = PubARLC { packet_type, packet_id, code, properties };
            return Ok((packet, n));
        }

        let (code, m) = {
            let (val, m) = u8::decode(advance(stream, n)?)?;
            (ReasCode::try_from(val)?, m)
        };
        let invalid_code = match (packet_type, code) {
            (PacketType::PubAck, ReasCode::PacketIdNotFound) => false,
            (PacketType::PubRec, ReasCode::PacketIdNotFound) => false,
            (PacketType::PubRel, ReasCode::Success) => true,
            (PacketType::PubRel, ReasCode::PacketIdNotFound) => true,
            (PacketType::PubRel, _) => false,
            (PacketType::PubComp, ReasCode::Success) => true,
            (PacketType::PubComp, ReasCode::PacketIdNotFound) => true,
            (PacketType::PubComp, _) => false,
            (_, _) => true,
        };
        if invalid_code {
            err!(MalformedPacket, code: MalformedPacket, "invalid code {:?}", code)?
        }
        n += m;

        if *fh.remaining_len < 4 {
            let packet = PubARLC { packet_type, packet_id, code, properties };
            return Ok((packet, n));
        }

        let (properties, m) = dec_props!(Properties, stream, n)?;
        n += m;

        let val = PubARLC { packet_type, packet_id, code, properties };
        Ok((val, n))
    }

    fn encode(&self) -> Result<Blob> {
        use crate::v5::insert_fixed_header;

        let mut data = Vec::with_capacity(64);

        data.extend_from_slice(self.packet_id.encode()?.as_ref());
        data.extend_from_slice((self.code as u8).encode()?.as_ref());
        if let Some(properties) = &self.properties {
            data.extend_from_slice(properties.encode()?.as_ref());
        } else {
            data.extend_from_slice(VarU32(0).encode()?.as_ref());
        }

        let remlen = VarU32(data.len().try_into()?);
        let fh = match self.packet_type {
            PacketType::PubRel => FixedHeader::new_pubrel(remlen)?,
            PacketType::PubRec => FixedHeader::new(PacketType::PubRec, remlen)?,
            PacketType::PubComp => FixedHeader::new(PacketType::PubComp, remlen)?,
            packet_type => err!(InvalidInput, desc: "packet_type {:?}", packet_type)?,
        };
        data = insert_fixed_header(fh, data)?;

        Ok(Blob::Large { data })
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Properties {
    pub reason_string: Option<String>,
    pub user_properties: Vec<UserProperty>,
}

impl Packetize for Properties {
    fn decode(stream: &[u8]) -> Result<(Self, usize)> {
        let mut dups = [false; 256];
        let mut props = Properties::default();

        let (len, mut n) = VarU32::decode(stream)?;
        let limit = usize::try_from(*len)? + n;

        while n < limit {
            let (property, m) = Property::decode(advance(stream, n)?)?;
            n += m;

            let pt = property.to_property_type();
            if pt != PropertyType::UserProp && dups[pt as usize] {
                err!(ProtocolError, code: ProtocolError, "duplicate property {:?}", pt)?
            }
            dups[pt as usize] = true;

            match property {
                Property::ReasonString(val) => {
                    props.reason_string = Some(val);
                }
                Property::UserProp(val) => {
                    props.user_properties.push(val);
                }
                _ => err!(
                    ProtocolError,
                    code: ProtocolError,
                    "{:?} found in puback properties",
                    pt
                )?,
            };
        }

        Ok((props, n))
    }

    fn encode(&self) -> Result<Blob> {
        use crate::{enc_prop, v5::insert_property_len};

        let mut data = Vec::with_capacity(64);

        enc_prop!(opt: data, ReasonString, &self.reason_string);

        for uprop in self.user_properties.iter() {
            enc_prop!(data, UserProp, uprop)
        }

        let data = insert_property_len(data.len(), data)?;

        Ok(Blob::Large { data })
    }
}