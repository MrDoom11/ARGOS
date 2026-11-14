use std::{io, ptr::NonNull, slice};

use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_NOT_FOUND, NO_ERROR};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GetIfEntry2, GetIfTable2Ex, GetIpForwardTable2, GetIpInterfaceEntry,
    IF_TYPE_SOFTWARE_LOOPBACK, MIB_IF_ROW2, MIB_IF_TABLE2, MIB_IPFORWARD_ROW2,
    MIB_IPFORWARD_TABLE2, MIB_IPINTERFACE_ROW, MibIfTableNormalWithoutStatistics,
};
use windows_sys::Win32::NetworkManagement::Ndis::{IfOperStatusUp, NET_LUID_LH};
use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6, AF_UNSPEC};
use windows_sys::core::GUID;

// Bits in MIB_IF_ROW2.InterfaceAndOperStatusFlags.
const HARDWARE_INTERFACE: u8 = 1;
const FILTER_INTERFACE: u8 = 1 << 1;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Interface {
    pub(crate) id: u128,
    pub(crate) name: String,
    pub(crate) connected: bool,
    luid: u64,
    hardware: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct Counters {
    pub(crate) received: u64,
    pub(crate) sent: u64,
    pub(crate) receive_bps: u64,
    pub(crate) transmit_bps: u64,
}

impl Interface {
    pub(crate) fn counters(&self) -> io::Result<Option<Counters>> {
        let mut row = MIB_IF_ROW2 {
            InterfaceLuid: NET_LUID_LH { Value: self.luid },
            ..Default::default()
        };
        match unsafe { GetIfEntry2(&mut row) } {
            NO_ERROR => Ok((interface_id(row.InterfaceGuid) == self.id
                && row.OperStatus == IfOperStatusUp)
                .then_some(Counters {
                    received: row.InOctets,
                    sent: row.OutOctets,
                    receive_bps: row.ReceiveLinkSpeed,
                    transmit_bps: row.TransmitLinkSpeed,
                })),
            ERROR_FILE_NOT_FOUND | ERROR_NOT_FOUND => Ok(None),
            status => Err(io::Error::from_raw_os_error(status as i32)),
        }
    }
}

pub(crate) fn interfaces() -> io::Result<Vec<Interface>> {
    let mut raw = std::ptr::null_mut();
    // Inventory needs only interface identity and status. Read statistics for
    // the selected interface separately.
    let status = unsafe { GetIfTable2Ex(MibIfTableNormalWithoutStatistics, &mut raw) };
    let table = MibTable::from_result(status, raw)?;
    let rows = unsafe {
        // Windows returns NumEntries contiguous rows after the aligned C header.
        table.rows::<MIB_IF_ROW2>(
            std::mem::offset_of!(MIB_IF_TABLE2, Table),
            (*table.0.as_ptr()).NumEntries,
        )
    };
    let mut interfaces: Vec<_> = rows
        .iter()
        .filter_map(|row| {
            let flags = row.InterfaceAndOperStatusFlags._bitfield;
            let id = interface_id(row.InterfaceGuid);
            let luid = unsafe { row.InterfaceLuid.Value };
            if id == 0
                || luid == 0
                || row.Type == IF_TYPE_SOFTWARE_LOOPBACK
                || flags & FILTER_INTERFACE != 0
            {
                return None;
            }
            let mut name = interface_name(&row.Alias);
            if name.is_empty() {
                name = interface_name(&row.Description);
            }
            if name.is_empty() {
                name = format!("Interface {}", row.InterfaceIndex);
            }
            Some(Interface {
                id,
                name,
                connected: row.OperStatus == IfOperStatusUp,
                luid,
                hardware: flags & HARDWARE_INTERFACE != 0,
            })
        })
        .collect();
    interfaces.sort_by_cached_key(|interface| {
        (
            !interface.connected,
            interface.name.to_lowercase(),
            interface.id,
        )
    });
    Ok(interfaces)
}

pub(crate) fn default_interface_id(interfaces: &[Interface]) -> Option<u128> {
    let first_connected = interfaces.iter().find(|interface| interface.connected)?;
    let best_route = best_default_route(interfaces);
    let connected_hardware = || {
        interfaces
            .iter()
            .find(|interface| interface.connected && interface.hardware)
            .map(|interface| interface.id)
    };
    best_route
        .or_else(connected_hardware)
        .or(Some(first_connected.id))
}

fn best_default_route(interfaces: &[Interface]) -> Option<u128> {
    let mut raw = std::ptr::null_mut();
    let status = unsafe { GetIpForwardTable2(AF_UNSPEC, &mut raw) };
    let table = MibTable::from_result(status, raw).ok()?;
    let rows = unsafe {
        table.rows::<MIB_IPFORWARD_ROW2>(
            std::mem::offset_of!(MIB_IPFORWARD_TABLE2, Table),
            (*table.0.as_ptr()).NumEntries,
        )
    };
    rows.iter()
        .filter_map(|route| {
            let family = default_family(route)?;
            let luid = unsafe { route.InterfaceLuid.Value };
            let interface = interfaces
                .iter()
                .find(|interface| interface.connected && interface.luid == luid)?;
            let mut ip_interface = MIB_IPINTERFACE_ROW {
                Family: family,
                InterfaceLuid: route.InterfaceLuid,
                ..Default::default()
            };
            if unsafe { GetIpInterfaceEntry(&mut ip_interface) } != NO_ERROR
                || ip_interface.DisableDefaultRoutes
            {
                return None;
            }
            let metric = u64::from(route.Metric) + u64::from(ip_interface.Metric);
            // Route costs are specific to their address family. Prefer IPv4
            // when both families have a usable default route.
            Some((family != AF_INET, metric, interface.id))
        })
        .min()
        .map(|(_, _, id)| id)
}

fn default_family(route: &MIB_IPFORWARD_ROW2) -> Option<u16> {
    let destination = route.DestinationPrefix;
    if destination.PrefixLength != 0 || route.ValidLifetime == 0 || route.Metric == u32::MAX {
        return None;
    }
    // Read only the union fields corresponding to the address family.
    unsafe {
        match destination.Prefix.si_family {
            AF_INET if destination.Prefix.Ipv4.sin_addr.S_un.S_addr == 0 => Some(AF_INET),
            AF_INET6 if destination.Prefix.Ipv6.sin6_addr.u.Byte == [0; 16] => Some(AF_INET6),
            _ => None,
        }
    }
}

fn interface_name(units: &[u16]) -> String {
    let len = units
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(units.len());
    String::from_utf16_lossy(&units[..len]).trim().to_owned()
}

fn interface_id(guid: GUID) -> u128 {
    (u128::from(guid.data1) << 96)
        | (u128::from(guid.data2) << 80)
        | (u128::from(guid.data3) << 64)
        | u128::from(u64::from_be_bytes(guid.data4))
}

// Both Windows table functions transfer the allocation to the caller, and
// neither table type's fixed Rust array describes its full runtime length.
struct MibTable<T>(NonNull<T>);

impl<T> MibTable<T> {
    fn from_result(status: u32, raw: *mut T) -> io::Result<Self> {
        if status != NO_ERROR {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        NonNull::new(raw)
            .map(Self)
            .ok_or_else(|| io::Error::other("Tabela de rede ausente"))
    }

    // The caller supplies the aligned offset and row count reported by Windows.
    unsafe fn rows<R>(&self, offset: usize, count: u32) -> &[R] {
        let first = unsafe { self.0.as_ptr().cast::<u8>().add(offset).cast::<R>() };
        unsafe { slice::from_raw_parts(first, count as usize) }
    }
}

impl<T> Drop for MibTable<T> {
    fn drop(&mut self) {
        unsafe { FreeMibTable(self.0.as_ptr().cast()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interface_guid_keeps_its_canonical_order() {
        let id = 0x12345678_abcd_9012_3456_9876543210abu128;
        assert_eq!(interface_id(GUID::from_u128(id)), id);
    }
}
