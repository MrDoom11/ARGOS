use std::net::SocketAddr;
use std::ptr;

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_INSUFFICIENT_BUFFER, ERROR_NOT_FOUND, ERROR_PRIVILEGE_NOT_HELD,
    NO_ERROR,
};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, GetPerTcp6ConnectionEStats, GetPerTcpConnectionEStats,
    MIB_TCP_STATE_ESTAB, MIB_TCP6ROW, MIB_TCP6ROW_OWNER_PID, MIB_TCP6TABLE_OWNER_PID,
    MIB_TCPROW_LH, MIB_TCPROW_LH_0, MIB_TCPROW_OWNER_PID, MIB_TCPTABLE_OWNER_PID,
    SetPerTcp6ConnectionEStats, SetPerTcpConnectionEStats, TCP_ESTATS_PATH_ROD_v0,
    TCP_ESTATS_PATH_RW_v0, TCP_TABLE_OWNER_PID_CONNECTIONS, TcpConnectionEstatsPath,
};
use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6, IN6_ADDR, IN6_ADDR_0};

use super::{Failure, Target};

pub(super) struct TcpRtt {
    target: Target,
    table: Vec<u32>,
    enable_result: Option<u32>,
}

impl TcpRtt {
    pub(super) fn new(target: Target) -> Self {
        Self {
            target,
            table: Vec::new(),
            enable_result: None,
        }
    }

    pub(super) fn sample(&mut self) -> Result<Option<f64>, Failure> {
        let Some(connection) = self.find_connection()? else {
            self.enable_result = None;
            return Ok(None);
        };
        match connection.read() {
            Ok(Some(stats)) => Ok((stats.CountRtt > 0).then_some(f64::from(stats.SmoothedRtt))),
            Ok(None) => {
                let status = *self
                    .enable_result
                    .get_or_insert_with(|| connection.enable());
                match status {
                    NO_ERROR => Ok(None),
                    ERROR_NOT_FOUND => {
                        self.enable_result = None;
                        Ok(None)
                    }
                    ERROR_ACCESS_DENIED | ERROR_PRIVILEGE_NOT_HELD => {
                        Err(Failure::PermissionDenied(
                            "O Windows exige privilégios de administrador para ativar as estatísticas TCP.",
                        ))
                    }
                    status => Err(api_error("ativar as estatísticas TCP", status)),
                }
            }
            Err(ERROR_NOT_FOUND) => {
                self.enable_result = None;
                Ok(None)
            }
            Err(status) => Err(api_error("ler as estatísticas TCP", status)),
        }
    }

    fn find_connection(&mut self) -> Result<Option<Connection>, Failure> {
        let pid = self.target.pid;
        let established = MIB_TCP_STATE_ESTAB as u32;
        match (self.target.local, self.target.remote) {
            (SocketAddr::V4(local), SocketAddr::V4(remote)) => {
                let table = self.read_table(AF_INET)?;
                // The AF_INET owner-PID table stores only C integer fields.
                let rows = unsafe {
                    table_rows::<MIB_TCPROW_OWNER_PID>(
                        table,
                        std::mem::offset_of!(MIB_TCPTABLE_OWNER_PID, table),
                    )
                };
                Ok(rows
                    .iter()
                    .find(|row| {
                        row.dwOwningPid == pid
                            && row.dwState == established
                            && row.dwLocalAddr.to_ne_bytes() == local.ip().octets()
                            && row.dwRemoteAddr.to_ne_bytes() == remote.ip().octets()
                            && u16::from_be(row.dwLocalPort as u16) == local.port()
                            && u16::from_be(row.dwRemotePort as u16) == remote.port()
                    })
                    .map(|row| {
                        Connection::V4(MIB_TCPROW_LH {
                            Anonymous: MIB_TCPROW_LH_0 {
                                dwState: row.dwState,
                            },
                            dwLocalAddr: row.dwLocalAddr,
                            dwLocalPort: row.dwLocalPort,
                            dwRemoteAddr: row.dwRemoteAddr,
                            dwRemotePort: row.dwRemotePort,
                        })
                    }))
            }
            (SocketAddr::V6(local), SocketAddr::V6(remote)) => {
                let table = self.read_table(AF_INET6)?;
                // The AF_INET6 owner-PID table stores only C integer fields.
                let rows = unsafe {
                    table_rows::<MIB_TCP6ROW_OWNER_PID>(
                        table,
                        std::mem::offset_of!(MIB_TCP6TABLE_OWNER_PID, table),
                    )
                };
                let mut matches = rows.iter().filter(|row| {
                    row.dwOwningPid == pid
                        && row.dwState == established
                        && row.ucLocalAddr == local.ip().octets()
                        && row.ucRemoteAddr == remote.ip().octets()
                        && u16::from_be(row.dwLocalPort as u16) == local.port()
                        && u16::from_be(row.dwRemotePort as u16) == remote.port()
                        && (local.scope_id() == 0
                            || u32::from_be(row.dwLocalScopeId) == local.scope_id())
                        && (remote.scope_id() == 0
                            || u32::from_be(row.dwRemoteScopeId) == remote.scope_id())
                });
                // The trace may omit a scope ID. In that case a match must be unique.
                let row = matches.next().filter(|_| matches.next().is_none());
                Ok(row.map(|row| {
                    Connection::V6(MIB_TCP6ROW {
                        State: MIB_TCP_STATE_ESTAB,
                        LocalAddr: IN6_ADDR {
                            u: IN6_ADDR_0 {
                                Byte: row.ucLocalAddr,
                            },
                        },
                        dwLocalScopeId: row.dwLocalScopeId,
                        dwLocalPort: row.dwLocalPort,
                        RemoteAddr: IN6_ADDR {
                            u: IN6_ADDR_0 {
                                Byte: row.ucRemoteAddr,
                            },
                        },
                        dwRemoteScopeId: row.dwRemoteScopeId,
                        dwRemotePort: row.dwRemotePort,
                    })
                }))
            }
            _ => Ok(None),
        }
    }

    fn read_table(&mut self, family: u16) -> Result<&[u32], Failure> {
        for _ in 0..4 {
            let mut size = size_of_val(self.table.as_slice()) as u32;
            let table = if size == 0 {
                ptr::null_mut()
            } else {
                self.table.as_mut_ptr().cast()
            };
            // The u32 buffer is aligned for both owner-PID table layouts.
            let status = unsafe {
                GetExtendedTcpTable(
                    table,
                    &mut size,
                    0,
                    u32::from(family),
                    TCP_TABLE_OWNER_PID_CONNECTIONS,
                    0,
                )
            };
            match status {
                NO_ERROR => return Ok(&self.table),
                ERROR_INSUFFICIENT_BUFFER => {
                    self.table
                        .resize((size as usize).div_ceil(size_of::<u32>()), 0);
                }
                status => return Err(api_error("consultar as conexões TCP", status)),
            }
        }
        Err(api_error(
            "consultar as conexões TCP",
            ERROR_INSUFFICIENT_BUFFER,
        ))
    }
}

// SAFETY: `R` must be the owner-PID row type corresponding to the table, aligned
// to at most four bytes and valid for all integer bit patterns.
unsafe fn table_rows<R>(table: &[u32], offset: usize) -> &[R] {
    let count = table.first().copied().unwrap_or(0) as usize;
    let bytes = size_of_val(table);
    if count == 0 || count > bytes.saturating_sub(offset) / size_of::<R>() {
        return &[];
    }
    // The SDK-provided offset is aligned and the checked row count fits the buffer.
    unsafe { std::slice::from_raw_parts(table.as_ptr().cast::<u8>().add(offset).cast(), count) }
}

enum Connection {
    V4(MIB_TCPROW_LH),
    V6(MIB_TCP6ROW),
}

impl Connection {
    fn read(&self) -> Result<Option<TCP_ESTATS_PATH_ROD_v0>, u32> {
        // Read the C BOOLEAN into a byte, then verify the collection status before using ROD.
        let mut enabled = 0u8;
        let mut path = TCP_ESTATS_PATH_ROD_v0::default();
        let rw_size = size_of::<TCP_ESTATS_PATH_RW_v0>() as u32;
        let rod_size = size_of::<TCP_ESTATS_PATH_ROD_v0>() as u32;
        let rod = ptr::from_mut(&mut path).cast();
        // Each pointer is to the corresponding Win32 structure for TcpConnectionEstatsPath v0.
        let status = unsafe {
            match self {
                Self::V4(row) => GetPerTcpConnectionEStats(
                    row,
                    TcpConnectionEstatsPath,
                    &mut enabled,
                    0,
                    rw_size,
                    ptr::null_mut(),
                    0,
                    0,
                    rod,
                    0,
                    rod_size,
                ),
                Self::V6(row) => GetPerTcp6ConnectionEStats(
                    row,
                    TcpConnectionEstatsPath,
                    &mut enabled,
                    0,
                    rw_size,
                    ptr::null_mut(),
                    0,
                    0,
                    rod,
                    0,
                    rod_size,
                ),
            }
        };
        match status {
            NO_ERROR => Ok((enabled == 1).then_some(path)),
            status => Err(status),
        }
    }

    fn enable(&self) -> u32 {
        let enabled = TCP_ESTATS_PATH_RW_v0 {
            EnableCollection: true,
        };
        let rw = ptr::from_ref(&enabled).cast();
        let size = size_of_val(&enabled) as u32;
        // Leave collection enabled for the connection lifetime so other readers keep their data.
        unsafe {
            match self {
                Self::V4(row) => {
                    SetPerTcpConnectionEStats(row, TcpConnectionEstatsPath, rw, 0, size, 0)
                }
                Self::V6(row) => {
                    SetPerTcp6ConnectionEStats(row, TcpConnectionEstatsPath, rw, 0, size, 0)
                }
            }
        }
    }
}

fn api_error(operation: &str, status: u32) -> Failure {
    Failure::Other(format!(
        "O Windows não conseguiu {operation} (código {status})."
    ))
}
