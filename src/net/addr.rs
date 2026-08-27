//! A `sockaddr` the kernel can be given a pointer to.
//!
//! Every operation that takes an address gives the kernel a raw pointer to it
//! and keeps that pointer until the completion arrives, so the address has to
//! live on the heap and be owned by the operation — the same rule buffers
//! follow, for the same reason.

use std::io;
use std::mem;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::ptr::NonNull;

/// A `sockaddr_storage` and the length that goes with it, boxed together.
#[repr(C)]
pub(crate) struct SockAddr {
    storage: libc::sockaddr_storage,
    /// Set to the storage size before an accept, and overwritten by the kernel
    /// with what it actually wrote.
    len: u32,
}

impl SockAddr {
    /// An empty address, sized for anything the kernel might return.
    pub(crate) fn zeroed() -> Box<SockAddr> {
        Box::new(SockAddr {
            // SAFETY: `sockaddr_storage` is plain data, and all zeroes is the
            // unspecified address family.
            storage: unsafe { mem::zeroed() },
            len: size_of::<libc::sockaddr_storage>() as u32,
        })
    }

    /// The kernel representation of `addr`.
    pub(crate) fn from_socket_addr(addr: SocketAddr) -> Box<SockAddr> {
        let mut this = SockAddr::zeroed();
        let storage = &raw mut this.storage;

        this.len = match addr {
            SocketAddr::V4(v4) => {
                let sin = libc::sockaddr_in {
                    sin_family: libc::AF_INET as libc::sa_family_t,
                    sin_port: v4.port().to_be(),
                    sin_addr: libc::in_addr {
                        // `octets` is already in network order, so reading them
                        // back as a native word puts the right bytes in memory.
                        s_addr: u32::from_ne_bytes(v4.ip().octets()),
                    },
                    sin_zero: [0; 8],
                };
                // SAFETY: `sockaddr_in` is smaller than `sockaddr_storage` and
                // no more aligned, which is what that type exists to guarantee.
                unsafe { storage.cast::<libc::sockaddr_in>().write(sin) };
                size_of::<libc::sockaddr_in>() as u32
            }
            SocketAddr::V6(v6) => {
                let sin6 = libc::sockaddr_in6 {
                    sin6_family: libc::AF_INET6 as libc::sa_family_t,
                    sin6_port: v6.port().to_be(),
                    sin6_flowinfo: v6.flowinfo(),
                    sin6_addr: libc::in6_addr {
                        s6_addr: v6.ip().octets(),
                    },
                    sin6_scope_id: v6.scope_id(),
                };
                // SAFETY: as above.
                unsafe { storage.cast::<libc::sockaddr_in6>().write(sin6) };
                size_of::<libc::sockaddr_in6>() as u32
            }
        };

        this
    }

    /// Read back what the kernel wrote.
    pub(crate) fn to_socket_addr(&self) -> io::Result<SocketAddr> {
        let storage = &raw const self.storage;

        match self.storage.ss_family as libc::c_int {
            libc::AF_INET => {
                // SAFETY: the family says the storage holds a `sockaddr_in`.
                let sin = unsafe { storage.cast::<libc::sockaddr_in>().read() };
                Ok(SocketAddr::V4(SocketAddrV4::new(
                    Ipv4Addr::from(sin.sin_addr.s_addr.to_ne_bytes()),
                    u16::from_be(sin.sin_port),
                )))
            }
            libc::AF_INET6 => {
                // SAFETY: as above, for `sockaddr_in6`.
                let sin6 = unsafe { storage.cast::<libc::sockaddr_in6>().read() };
                Ok(SocketAddr::V6(SocketAddrV6::new(
                    Ipv6Addr::from(sin6.sin6_addr.s6_addr),
                    u16::from_be(sin6.sin6_port),
                    sin6.sin6_flowinfo,
                    sin6.sin6_scope_id,
                )))
            }
            other => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported address family {other}"),
            )),
        }
    }

    /// The address family to open a socket in.
    pub(crate) fn domain(addr: SocketAddr) -> i32 {
        match addr {
            SocketAddr::V4(_) => libc::AF_INET,
            SocketAddr::V6(_) => libc::AF_INET6,
        }
    }

    /// The length the kernel should read, for an operation that takes it by
    /// value rather than by pointer.
    pub(crate) fn len(&self) -> u32 {
        self.len
    }

    /// Pointers to the address and to its length, as an accept wants them.
    ///
    /// Raw from the start rather than derived from two overlapping borrows, so
    /// that handing both to the kernel at once is unambiguous.
    pub(crate) fn ptrs(&mut self) -> (NonNull<libc::c_void>, NonNull<u32>) {
        let this = &raw mut *self;
        // SAFETY: both fields exist and are non-null; they are distinct, so the
        // kernel writing through one does not disturb the other.
        unsafe {
            (
                NonNull::new_unchecked((&raw mut (*this).storage).cast()),
                NonNull::new_unchecked(&raw mut (*this).len),
            )
        }
    }

    /// A pointer to the address alone, for connect and bind.
    pub(crate) fn addr_ptr(&mut self) -> NonNull<libc::c_void> {
        self.ptrs().0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ipv4_address_survives_the_round_trip() {
        let addr: SocketAddr = "192.0.2.17:8080".parse().unwrap();
        let encoded = SockAddr::from_socket_addr(addr);

        assert_eq!(encoded.len(), size_of::<libc::sockaddr_in>() as u32);
        assert_eq!(encoded.to_socket_addr().unwrap(), addr);
    }

    #[test]
    fn an_ipv6_address_survives_the_round_trip() {
        let addr: SocketAddr = "[2001:db8::1]:443".parse().unwrap();
        let encoded = SockAddr::from_socket_addr(addr);

        assert_eq!(encoded.len(), size_of::<libc::sockaddr_in6>() as u32);
        assert_eq!(encoded.to_socket_addr().unwrap(), addr);
    }

    /// The port has to reach the wire big-endian whatever the host does.
    #[test]
    fn the_port_is_stored_in_network_order() {
        let encoded = SockAddr::from_socket_addr("127.0.0.1:258".parse().unwrap());
        // SAFETY: it was just written as a `sockaddr_in`.
        let sin = unsafe {
            (&raw const encoded.storage)
                .cast::<libc::sockaddr_in>()
                .read()
        };

        assert_eq!(sin.sin_port.to_ne_bytes(), [1, 2], "258 == 0x0102");
    }

    #[test]
    fn an_unset_address_is_not_mistaken_for_one() {
        assert!(SockAddr::zeroed().to_socket_addr().is_err());
    }

    #[test]
    fn a_fresh_address_offers_the_whole_storage_to_the_kernel() {
        let mut addr = SockAddr::zeroed();
        let (storage, len) = addr.ptrs();

        // SAFETY: both point into the box, which is still alive.
        assert_eq!(unsafe { *len.as_ptr() }, 128);
        assert_eq!(storage.as_ptr().cast::<u8>(), (&raw mut *addr).cast());
    }
}
