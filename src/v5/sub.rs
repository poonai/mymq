#[cfg(any(feature = "fuzzy", test))]
use arbitrary::{Arbitrary, Error as ArbitraryError, Unstructured};

#[cfg(any(feature = "fuzzy", test))]
use std::result;

use crate::v5::{FixedHeader, Property, PropertyType, QoS};
use crate::{util::advance, Blob, Packetize, TopicFilter, UserProperty, VarU32};
use crate::{Error, ErrorKind, ReasonCode, Result};

const PP: &'static str = "Packet::Subscribe";

/// Subscription options carried in SUBSCRIBE Packet
#[derive(Clone, Eq, PartialEq, Debug)]
pub struct SubscriptionOpt(u8);

#[cfg(any(feature = "fuzzy", test))]
impl<'a> Arbitrary<'a> for SubscriptionOpt {
    fn arbitrary(uns: &mut Unstructured<'a>) -> result::Result<Self, ArbitraryError> {
        let rfr: RetainForwardRule = uns.arbitrary()?;
        let rap: bool = uns.arbitrary()?;
        let nl: bool = uns.arbitrary()?;
        let qos: QoS = uns.arbitrary()?;

        Ok(SubscriptionOpt::new(rfr, rap, nl, qos))
    }
}

impl Packetize for SubscriptionOpt {
    fn decode<T: AsRef<[u8]>>(stream: T) -> Result<(Self, usize)> {
        let stream: &[u8] = stream.as_ref();

        let (opt, n) = dec_field!(u8, stream, 0);
        let val = SubscriptionOpt(opt);

        val.validate()?;
        Ok((val, n))
    }

    fn encode(&self) -> Result<Blob> {
        self.validate()?;
        self.0.encode()
    }
}

impl SubscriptionOpt {
    const MAXIMUM_QOS: u8 = 0b0000_0011;
    const NO_LOCAL: u8 = 0b0000_0100;
    const RETAIN_AS_PUBLISHED: u8 = 0b0000_1000;
    const RETAIN_HANDLING: u8 = 0b0011_0000;
    const RESERVED: u8 = 0b1100_0000;

    pub fn new(rfr: RetainForwardRule, rap: bool, nl: bool, qos: QoS) -> Self {
        let rfr: u8 = u8::from(rfr) << 4;
        let rap: u8 = if rap { 0b1000 } else { 0b0000 };
        let nl: u8 = if nl { 0b0100 } else { 0b0000 };
        let qos: u8 = qos.into();

        SubscriptionOpt(rfr | rap | nl | qos)
    }

    /// Return (retain_forward_rule, retain_as_published, no_local, qos)
    pub fn unwrap(&self) -> (RetainForwardRule, bool, bool, QoS) {
        let qos: QoS = (self.0 & Self::MAXIMUM_QOS).try_into().unwrap();
        let nl: bool = (self.0 & Self::NO_LOCAL) > 0;
        let rap: bool = (self.0 & Self::RETAIN_AS_PUBLISHED) > 0;
        (
            RetainForwardRule::try_from((self.0 >> 4) & Self::RETAIN_HANDLING).unwrap(),
            rap,
            nl,
            qos,
        )
    }

    fn validate(&self) -> Result<()> {
        Ok(())
    }
}

/// RetainForwardRule part of Subscription option defined by MQTT spec.
#[cfg_attr(any(feature = "fuzzy", test), derive(Arbitrary))]
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum RetainForwardRule {
    OnEverySubscribe = 0,
    OnNewSubscribe = 1,
    Never = 2,
}

impl TryFrom<u8> for RetainForwardRule {
    type Error = Error;

    fn try_from(val: u8) -> Result<RetainForwardRule> {
        let val = match val {
            0 => RetainForwardRule::OnEverySubscribe,
            1 => RetainForwardRule::OnNewSubscribe,
            2 => RetainForwardRule::Never,
            _ => err!(MalformedPacket, code: MalformedPacket, "{} forbidden packet", PP)?,
        };

        Ok(val)
    }
}

impl From<RetainForwardRule> for u8 {
    fn from(val: RetainForwardRule) -> u8 {
        match val {
            RetainForwardRule::OnEverySubscribe => 0,
            RetainForwardRule::OnNewSubscribe => 1,
            RetainForwardRule::Never => 2,
        }
    }
}

/// SUBSCRIBE Packet
#[derive(Clone, Eq, PartialEq, Debug)]
pub struct Subscribe {
    pub packet_id: u16,
    pub properties: Option<SubscribeProperties>,
    pub filters: Vec<SubscribeFilter>,
}

#[cfg(any(feature = "fuzzy", test))]
impl<'a> Arbitrary<'a> for Subscribe {
    fn arbitrary(uns: &mut Unstructured<'a>) -> result::Result<Self, ArbitraryError> {
        let mut filters: Vec<SubscribeFilter> = vec![];
        for _i in 0..((uns.arbitrary::<u8>()? % 32) + 1) {
            filters.push(uns.arbitrary()?)
        }

        let val = Subscribe {
            packet_id: uns.arbitrary()?,
            properties: uns.arbitrary()?,
            filters,
        };

        Ok(val)
    }
}

impl Packetize for Subscribe {
    fn decode<T: AsRef<[u8]>>(stream: T) -> Result<(Self, usize)> {
        let stream: &[u8] = stream.as_ref();

        let (fh, fh_len) = dec_field!(FixedHeader, stream, 0);
        fh.validate()?;

        let (packet_id, n) = dec_field!(u16, stream, fh_len);
        let (properties, n) = dec_props!(SubscribeProperties, stream, n);
        let (payload, n) = match fh_len + usize::try_from(*fh.remaining_len)? {
            m if m == n => {
                err!(MalformedPacket, code: MalformedPacket, "{} in payload {}", PP, m)?
            }
            m if m <= stream.len() => (&stream[n..m], m),
            m => err!(MalformedPacket, code: MalformedPacket, "{} in payload {}", PP, m)?,
        };

        // Assume each entry will take 32 bytes.
        let mut filters = Vec::with_capacity((payload.len() / 32) + 1);
        let mut t = 0;
        while t < payload.len() {
            let (filter, m) = dec_field!(SubscribeFilter, payload, t);
            t = m;
            filters.push(filter);
        }

        let val = Subscribe { packet_id, properties, filters };

        val.validate()?;
        Ok((val, n))
    }

    fn encode(&self) -> Result<Blob> {
        use crate::v5::insert_fixed_header;

        self.validate()?;

        let mut data = Vec::with_capacity(64);

        data.extend_from_slice(self.packet_id.encode()?.as_ref());
        if let Some(properties) = &self.properties {
            data.extend_from_slice(properties.encode()?.as_ref());
        } else {
            data.extend_from_slice(VarU32(0).encode()?.as_ref());
        }

        for filter in self.filters.iter() {
            data.extend_from_slice(filter.encode()?.as_ref());
        }

        let fh = FixedHeader::new_subscribe(VarU32(data.len().try_into()?))?;
        data = insert_fixed_header(fh, data)?;

        Ok(Blob::Large { data })
    }
}

impl Subscribe {
    #[cfg(any(feature = "fuzzy", test))]
    pub fn normalize(&mut self) {
        if let Some(props) = &mut self.properties {
            if props.is_empty() {
                self.properties = None
            }
        }
    }

    fn validate(&self) -> Result<()> {
        if self.filters.len() == 0 {
            err!(ProtocolError, code: ProtocolError, "{} missing topic filter", PP)?
        }

        for filter in self.filters.iter() {
            if (filter.opt.0 & SubscriptionOpt::RESERVED) > 0 {
                err!(
                    MalformedPacket,
                    code: MalformedPacket,
                    "{} sub-opt reserved bit != 0 0x{:x}",
                    PP,
                    filter.opt.0
                )?
            } else if ((filter.opt.0 & SubscriptionOpt::RETAIN_HANDLING) >> 4) == 3 {
                err!(
                    MalformedPacket,
                    code: MalformedPacket,
                    "{} invalid retain handling 0x{:x}",
                    PP,
                    filter.opt.0
                )?
            }
            QoS::try_from(filter.opt.0 & SubscriptionOpt::MAXIMUM_QOS)?;
        }

        Ok(())
    }
}

/// Collection of MQTT properties allowed in SUBSCRIBE packet
#[derive(Debug, Clone, Eq, PartialEq, Default)]
pub struct SubscribeProperties {
    pub subscription_id: Option<VarU32>,
    pub user_properties: Vec<UserProperty>,
}

#[cfg(any(feature = "fuzzy", test))]
impl<'a> Arbitrary<'a> for SubscribeProperties {
    fn arbitrary(uns: &mut Unstructured<'a>) -> result::Result<Self, ArbitraryError> {
        use crate::types;

        let n_user_props = uns.arbitrary::<usize>()? % 4;
        let val = SubscribeProperties {
            subscription_id: uns.arbitrary()?,
            user_properties: types::valid_user_props(uns, n_user_props)?,
        };

        Ok(val)
    }
}

impl Packetize for SubscribeProperties {
    fn decode<T: AsRef<[u8]>>(stream: T) -> Result<(Self, usize)> {
        use crate::v5::Property::*;

        let stream: &[u8] = stream.as_ref();

        let mut dups = [false; 256];
        let mut props = SubscribeProperties::default();

        let (len, mut n) = dec_field!(VarU32, stream, 0);
        let limit = usize::try_from(*len)? + n;

        while n < limit {
            let (property, m) = dec_field!(Property, stream, n);
            n = m;

            let pt = property.to_property_type();
            if pt != PropertyType::UserProp && dups[pt as usize] {
                err!(ProtocolError, code: ProtocolError, "{} repeat prop {:?}", PP, pt)?
            }
            dups[pt as usize] = true;

            match property {
                SubscriptionIdentifier(VarU32(0)) => {
                    err!(ProtocolError, code: ProtocolError, "{} subcr_ide:0", PP)?;
                }
                SubscriptionIdentifier(val) => props.subscription_id = Some(val),
                UserProp(val) => props.user_properties.push(val),
                _ => {
                    err!(ProtocolError, code: ProtocolError, "{} bad prop {:?}", PP, pt)?
                }
            }
        }

        Ok((props, n))
    }

    fn encode(&self) -> Result<Blob> {
        use crate::v5::insert_property_len;

        let mut data = Vec::with_capacity(64);

        enc_prop!(opt: data, SubscriptionIdentifier, self.subscription_id);

        for uprop in self.user_properties.iter() {
            enc_prop!(data, UserProp, uprop);
        }

        data = insert_property_len(data.len(), data)?;

        Ok(Blob::Large { data })
    }
}

impl SubscribeProperties {
    #[cfg(any(feature = "fuzzy", test))]
    pub fn is_empty(&self) -> bool {
        self.subscription_id.is_none() && self.user_properties.len() == 0
    }
}

/// SubscribeFilter defined in the SUBSCRIBE packet's payload.
#[derive(Clone, Eq, PartialEq, Debug)]
pub struct SubscribeFilter {
    pub topic_filter: TopicFilter,
    pub opt: SubscriptionOpt,
}

#[cfg(any(feature = "fuzzy", test))]
impl<'a> Arbitrary<'a> for SubscribeFilter {
    fn arbitrary(uns: &mut Unstructured<'a>) -> result::Result<Self, ArbitraryError> {
        let val = SubscribeFilter {
            topic_filter: uns.arbitrary()?,
            opt: uns.arbitrary()?,
        };

        Ok(val)
    }
}

impl Packetize for SubscribeFilter {
    fn decode<T: AsRef<[u8]>>(stream: T) -> Result<(Self, usize)> {
        let stream: &[u8] = stream.as_ref();

        let (topic_filter, n) = dec_field!(TopicFilter, stream, 0);
        let (opt, n) = dec_field!(SubscriptionOpt, stream, n);

        let val = SubscribeFilter { topic_filter, opt };

        val.validate()?;
        Ok((val, n))
    }

    fn encode(&self) -> Result<Blob> {
        let mut data = Vec::with_capacity(64);

        data.extend_from_slice(self.topic_filter.encode()?.as_ref());
        data.extend_from_slice(self.opt.encode()?.as_ref());

        Ok(Blob::Large { data })
    }
}

impl SubscribeFilter {
    fn validate(&self) -> Result<()> {
        Ok(())
    }
}
