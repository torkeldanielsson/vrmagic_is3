//! The controller's Ethernet interface on the camera LAN.
use anyhow::{Context, Result, bail};
use std::net::Ipv4Addr;

pub struct Interface {
    /// The system name: `eth0`, `en0`, or the adapter GUID on Windows.
    pub name: String,
    pub index: u32,
    pub ipv4: Option<Ipv4Addr>,
    pub mac: [u8; 6],
}

fn ethernet_mac(interface: &netdev::Interface) -> Option<[u8; 6]> {
    interface
        .mac_addr
        .map(|mac| mac.octets())
        .filter(|mac| *mac != [0; 6])
}

/// Finds an interface by its system name (`eth0`, `en0`) or its adapter name
/// (`Ethernet` on Windows, where the system name is a GUID).
pub fn interface(name: &str) -> Result<Interface> {
    let interfaces = netdev::get_interfaces();
    let Some(found) = interfaces
        .iter()
        .find(|i| i.name == name || i.friendly_name.as_deref() == Some(name))
    else {
        let candidates: Vec<_> = interfaces
            .iter()
            .filter(|i| ethernet_mac(i).is_some())
            .map(|i| match &i.friendly_name {
                Some(adapter) if cfg!(windows) => adapter.as_str(),
                _ => &i.name,
            })
            .collect();
        bail!(
            "Unknown interface {name}; candidates: {}",
            candidates.join(", ")
        );
    };
    Ok(Interface {
        name: found.name.clone(),
        index: found.index,
        ipv4: found.ipv4.first().map(|network| network.addr()),
        mac: ethernet_mac(found).context("Choose an Ethernet interface")?,
    })
}
