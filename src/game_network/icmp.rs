use std::io;
use std::mem::MaybeUninit;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    ICMP_ECHO_REPLY, ICMPV6_ECHO_REPLY_LH, IP_REQ_TIMED_OUT, IP_SUCCESS, Icmp6CreateFile,
    Icmp6SendEcho2, IcmpCloseHandle, IcmpCreateFile, IcmpSendEcho, IcmpSendEcho2Ex,
};
use windows_sys::Win32::Networking::WinSock::{
    AF_INET6, IN6_ADDR, IN6_ADDR_0, SOCKADDR_IN6, SOCKADDR_IN6_0,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

use super::Target;

const TIMEOUT_MS: u32 = 350;
const PAYLOAD: &[u8] = b"ARGOS";
const REPLY_EXTRA: usize = PAYLOAD.len() + 8 + size_of::<IO_STATUS_BLOCK>();

pub(super) struct IcmpRtt {
    local: Option<IpAddr>,
    remote: IpAddr,
    scope: u32,
    handle: Option<EchoHandle>,
}

struct EchoHandle(HANDLE);

impl Drop for EchoHandle {
    fn drop(&mut self) {
        unsafe { IcmpCloseHandle(self.0) };
    }
}

#[repr(C)]
struct Reply<T> {
    header: T,
    extra: [u8; REPLY_EXTRA],
}

impl IcmpRtt {
    pub(super) fn new(target: Target) -> Self {
        Self {
            local: Some(target.local.ip().to_canonical()),
            remote: target.remote.ip().to_canonical(),
            scope: 0,
            handle: None,
        }
    }

    pub(super) fn for_address(address: SocketAddr) -> Self {
        Self {
            local: None,
            remote: address.ip().to_canonical(),
            scope: match address {
                SocketAddr::V4(_) => 0,
                SocketAddr::V6(address) => address.scope_id(),
            },
            handle: None,
        }
    }

    pub(super) fn sample(&mut self) -> Result<Option<f64>, String> {
        if self.handle.is_none() {
            let handle = unsafe {
                if self.remote.is_ipv4() {
                    IcmpCreateFile()
                } else {
                    Icmp6CreateFile()
                }
            };
            if handle == INVALID_HANDLE_VALUE {
                return Err(format!(
                    "sonda indisponível ({})",
                    io::Error::last_os_error()
                ));
            }
            self.handle = Some(EchoHandle(handle));
        }
        let handle = self.handle.as_ref().expect("sonda ICMP aberta").0;

        let (status, rtt) = match self.remote {
            IpAddr::V4(destination) => {
                let mut reply = MaybeUninit::<Reply<ICMP_ECHO_REPLY>>::zeroed();
                let destination = u32::from_ne_bytes(destination.octets());
                // No event or callback is supplied, so the buffer stays valid until completion.
                let count = unsafe {
                    if let Some(local) = self.local {
                        let source = match local {
                            IpAddr::V4(address) => u32::from_ne_bytes(address.octets()),
                            IpAddr::V6(_) => 0,
                        };
                        IcmpSendEcho2Ex(
                            handle,
                            null_mut(),
                            None,
                            null(),
                            source,
                            destination,
                            PAYLOAD.as_ptr().cast(),
                            PAYLOAD.len() as u16,
                            null(),
                            reply.as_mut_ptr().cast(),
                            size_of::<Reply<ICMP_ECHO_REPLY>>() as u32,
                            TIMEOUT_MS,
                        )
                    } else {
                        IcmpSendEcho(
                            handle,
                            destination,
                            PAYLOAD.as_ptr().cast(),
                            PAYLOAD.len() as u16,
                            null(),
                            reply.as_mut_ptr().cast(),
                            size_of::<Reply<ICMP_ECHO_REPLY>>() as u32,
                            TIMEOUT_MS,
                        )
                    }
                };
                if count == 0 {
                    return Err(no_reply());
                }
                let header = unsafe { reply.assume_init() }.header;
                (header.Status, header.RoundTripTime)
            }
            IpAddr::V6(destination) => {
                let source = match self.local {
                    Some(IpAddr::V6(address)) => address,
                    _ => Ipv6Addr::UNSPECIFIED,
                };
                let mut reply = MaybeUninit::<Reply<ICMPV6_ECHO_REPLY_LH>>::zeroed();
                let count = unsafe {
                    Icmp6SendEcho2(
                        handle,
                        null_mut(),
                        None,
                        null(),
                        &socket_address(source, 0),
                        &socket_address(destination, self.scope),
                        PAYLOAD.as_ptr().cast(),
                        PAYLOAD.len() as u16,
                        null(),
                        reply.as_mut_ptr().cast(),
                        size_of::<Reply<ICMPV6_ECHO_REPLY_LH>>() as u32,
                        TIMEOUT_MS,
                    )
                };
                if count == 0 {
                    return Err(no_reply());
                }
                let header = unsafe { reply.assume_init() }.header;
                (header.Status, header.RoundTripTime)
            }
        };
        match status {
            IP_SUCCESS => Ok(Some(f64::from(rtt))),
            IP_REQ_TIMED_OUT => Err("sem resposta".to_owned()),
            _ => Err(format!("sem resposta válida (código {status})")),
        }
    }
}

fn socket_address(address: Ipv6Addr, scope: u32) -> SOCKADDR_IN6 {
    SOCKADDR_IN6 {
        sin6_family: AF_INET6,
        sin6_addr: IN6_ADDR {
            u: IN6_ADDR_0 {
                Byte: address.octets(),
            },
        },
        Anonymous: SOCKADDR_IN6_0 {
            sin6_scope_id: scope,
        },
        ..Default::default()
    }
}

fn no_reply() -> String {
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(IP_REQ_TIMED_OUT as i32) {
        "sem resposta".to_owned()
    } else {
        format!("sonda indisponível ({error})")
    }
}
