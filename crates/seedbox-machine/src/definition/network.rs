//! A machine's optional Ethernet interface on the simulation's network.
//!
// TODO: Re-evaluate: addressing is explicit in the definition because routing
// and guest bootstrap need it before execution, and index-derived addresses
// would change with spawn or host-addition order. One optional interface on
// one isolated L2 segment is the minimal surface; multiple NICs need a new design.
use super::DefinitionError;
use std::{fmt, net::Ipv4Addr};

/// A unicast, nonzero Ethernet address assigned to one interface.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MacAddress([u8; 6]);

impl MacAddress {
    /// Accept a unicast address: the group (multicast) bit of the first octet
    /// must be clear and the address must not be all zero.
    pub fn new(octets: [u8; 6]) -> Result<Self, DefinitionError> {
        if octets[0] & 1 != 0 {
            return Err(DefinitionError(format!(
                "MAC address {} is not unicast",
                Self(octets)
            )));
        }
        if octets == [0; 6] {
            return Err(DefinitionError("MAC address must not be zero".into()));
        }
        Ok(Self(octets))
    }

    /// The address in transmission order.
    pub fn octets(self) -> [u8; 6] {
        self.0
    }
}

impl fmt::Display for MacAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [a, b, c, d, e, g] = self.0;
        write!(f, "{a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{g:02x}")
    }
}

/// The machine's single Ethernet interface (`eth0`) on the simulation's one
/// isolated L2 segment. The guest is bootstrapped with this static IPv4
/// address and prefix; there is no gateway, DNS or DHCP. The guest may use
/// any Ethernet protocol. A definition without an interface has no NIC.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetworkInterface {
    mac: MacAddress,
    ipv4: Ipv4Addr,
    prefix_len: u8,
}

impl NetworkInterface {
    /// Accept a prefix length in 0..=32. The address is used as given; the
    /// MAC address is validated by [`MacAddress::new`].
    pub fn new(mac: MacAddress, ipv4: Ipv4Addr, prefix_len: u8) -> Result<Self, DefinitionError> {
        if prefix_len > 32 {
            return Err(DefinitionError(format!(
                "IPv4 prefix length {prefix_len} exceeds 32"
            )));
        }
        Ok(Self {
            mac,
            ipv4,
            prefix_len,
        })
    }

    /// The interface's Ethernet address, unique within one simulation.
    pub fn mac(&self) -> MacAddress {
        self.mac
    }

    /// The static IPv4 address configured at boot.
    pub fn ipv4(&self) -> Ipv4Addr {
        self.ipv4
    }

    /// The IPv4 prefix length configured at boot.
    pub fn prefix_len(&self) -> u8 {
        self.prefix_len
    }
}

#[cfg(test)]
mod tests;
