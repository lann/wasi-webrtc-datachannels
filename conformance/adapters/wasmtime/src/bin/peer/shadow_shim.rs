//! In-binary overrides of the libc syscall wrappers whose Shadow-simulator
//! behavior breaks the `webrtc` 0.20 driver's quinn-udp UDP layer.
//!
//! Symbols defined in the executable win dynamic-symbol resolution over every
//! preloaded library — including Shadow's own `preload-libc`, which Shadow
//! always prepends to `LD_PRELOAD` — so overriding here is the one
//! interposition point that works identically inside and outside Shadow, with
//! no environment plumbing.
//!
//! Every override **forwards the call as received** first, through
//! `dlsym(RTLD_NEXT)` (inside Shadow that resolves to Shadow's `preload-libc`,
//! keeping the forwarded call on Shadow's fast interposition path; elsewhere it
//! resolves to libc). Only when the forwarded call fails with the **exact error
//! Shadow's unimplemented path produces** does the stub engage:
//!
//! - `setsockopt(IPPROTO_IP, {IP_PKTINFO, IP_MTU_DISCOVER, IP_RECVTOS})` —
//!   Shadow rejects socket options it does not implement with `ENOPROTOOPT`
//!   (`shadow/src/main/host/descriptor/socket/inet/udp.rs`), and quinn-udp
//!   treats an `IP_PKTINFO` failure as fatal to socket construction. The stub
//!   reports success: these options only enable optional receive metadata
//!   (destination-address / TOS control messages) that Shadow never delivers
//!   anyway (its `recvmsg` returns `control_len: 0`), a shape quinn-udp
//!   already handles (`RecvMeta { dst_ip: None, .. }`). A targeted option
//!   failing with any *other* error is neither the real-kernel success nor
//!   Shadow's documented rejection: the override aborts the peer with a
//!   diagnostic rather than stub over an unknown environment.
//! - `recvmmsg` — Shadow's syscall handler does not implement it (its
//!   dispatch covers `recvmsg`/`sendmsg`/`recvfrom`/`sendto` only, so the call
//!   fails with `ENOSYS`), and quinn-udp's Linux receive path calls it
//!   unconditionally with no fallback. After `ENOSYS` is observed once, the
//!   stub emulates a one-message batch via `recvmsg` (a valid short batch —
//!   callers must handle fewer messages than requested). Other errors
//!   (`EAGAIN`, ...) are legitimate runtime results and are forwarded
//!   untouched.
//!
//! Scope: IPv4 options only — the Shadow lab addresses its hosts with IPv4 —
//! and exactly the syscalls quinn-udp 0.6 uses. If a future `webrtc`/quinn-udp
//! bump grows the syscall surface (IPv6 metadata options, `sendmmsg`, ...),
//! the symptom is the peer aborting (targeted `setsockopt`) or Shadow's
//! "unsupported syscall" warning plus a hang (a new unimplemented syscall),
//! and this module is where to extend the bridge.

use std::ffi::{c_int, c_uint, c_void, CStr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

/// Resolve the next definition of `name` after this executable (Shadow's
/// `preload-libc` when present, else libc), caching the address in `cell`.
fn next_fn(cell: &OnceLock<usize>, name: &'static CStr) -> usize {
    *cell.get_or_init(|| {
        let addr = unsafe { libc::dlsym(libc::RTLD_NEXT, name.as_ptr()) };
        assert!(
            !addr.is_null(),
            "shadow-syscall-shim: dlsym(RTLD_NEXT, {name:?}) found no next definition"
        );
        addr as usize
    })
}

/// Whether this `setsockopt` target is one Shadow rejects but quinn-udp needs
/// to "succeed": the optional-receive-metadata options on IPv4 UDP sockets.
fn stubbed_ip_option(level: c_int, optname: c_int) -> bool {
    level == libc::IPPROTO_IP
        && matches!(
            optname,
            libc::IP_PKTINFO | libc::IP_MTU_DISCOVER | libc::IP_RECVTOS
        )
}

type SetsockoptFn =
    unsafe extern "C" fn(c_int, c_int, c_int, *const c_void, libc::socklen_t) -> c_int;

/// Override of libc `setsockopt`: forward as received; report success when a
/// targeted receive-metadata option fails with Shadow's `ENOPROTOOPT`; abort
/// on any other targeted failure.
///
/// # Safety
///
/// Same contract as libc `setsockopt`; called by foreign code through the
/// dynamic linker.
#[no_mangle]
pub unsafe extern "C" fn setsockopt(
    fd: c_int,
    level: c_int,
    optname: c_int,
    optval: *const c_void,
    optlen: libc::socklen_t,
) -> c_int {
    static NEXT: OnceLock<usize> = OnceLock::new();
    let next: SetsockoptFn = unsafe { std::mem::transmute(next_fn(&NEXT, c"setsockopt")) };

    let ret = unsafe { next(fd, level, optname, optval, optlen) };
    if ret == 0 || !stubbed_ip_option(level, optname) {
        return ret;
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ENOPROTOOPT) {
        // Shadow's documented rejection of an option it does not implement:
        // report success. The option only enables receive metadata that is
        // simply never delivered, which the callers handle.
        return 0;
    }
    eprintln!(
        "shadow-syscall-shim: setsockopt(fd={fd}, level={level}, opt={optname}) failed with \
         unexpected error {err} (expected success on a real kernel or ENOPROTOOPT under \
         Shadow); aborting"
    );
    std::process::abort();
}

type RecvmmsgFn =
    unsafe extern "C" fn(c_int, *mut libc::mmsghdr, c_uint, c_int, *mut libc::timespec) -> c_int;
type RecvmsgFn = unsafe extern "C" fn(c_int, *mut libc::msghdr, c_int) -> libc::ssize_t;

/// Override of libc `recvmmsg`: forward as received; once a forwarded call
/// fails with Shadow's `ENOSYS`, emulate a one-message batch via `recvmsg`
/// from then on. Any other outcome — success or a genuine runtime error such
/// as `EAGAIN` — is forwarded untouched.
///
/// The emulation ignores `timeout`: it receives at most one message, and
/// `recvmmsg`'s timeout only bounds waiting *between* messages of a batch
/// (quinn-udp always passes null).
///
/// # Safety
///
/// Same contract as libc `recvmmsg`; called by foreign code through the
/// dynamic linker.
#[no_mangle]
pub unsafe extern "C" fn recvmmsg(
    fd: c_int,
    msgvec: *mut libc::mmsghdr,
    vlen: c_uint,
    flags: c_int,
    timeout: *mut libc::timespec,
) -> c_int {
    static NEXT: OnceLock<usize> = OnceLock::new();
    static ENOSYS_SEEN: AtomicBool = AtomicBool::new(false);

    if !ENOSYS_SEEN.load(Ordering::Relaxed) {
        let next: RecvmmsgFn = unsafe { std::mem::transmute(next_fn(&NEXT, c"recvmmsg")) };
        let ret = unsafe { next(fd, msgvec, vlen, flags, timeout) };
        if ret >= 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ENOSYS) {
            return ret;
        }
        // Shadow's unimplemented-syscall error, observed: emulate from now on.
        ENOSYS_SEEN.store(true, Ordering::Relaxed);
    }

    if msgvec.is_null() || vlen == 0 {
        unsafe { *libc::__errno_location() = libc::EINVAL };
        return -1;
    }
    static NEXT_RECVMSG: OnceLock<usize> = OnceLock::new();
    let next_recvmsg: RecvmsgFn =
        unsafe { std::mem::transmute(next_fn(&NEXT_RECVMSG, c"recvmsg")) };
    // `MSG_WAITFORONE` is recvmmsg-only; the emulated batch is one message, so
    // its "return after the first message" semantics hold trivially.
    let flags = flags & !libc::MSG_WAITFORONE;
    let msg = unsafe { &mut *msgvec };
    let n = unsafe { next_recvmsg(fd, &mut msg.msg_hdr, flags) };
    if n < 0 {
        return -1;
    }
    msg.msg_len = n as c_uint;
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::IoSliceMut;
    use std::mem::MaybeUninit;
    use std::net::UdpSocket;
    use std::os::fd::AsRawFd as _;

    /// Install a thread-scoped seccomp-BPF filter replicating Shadow's
    /// behavior for the syscalls this shim bridges:
    /// `setsockopt(IPPROTO_IP, {IP_PKTINFO, IP_MTU_DISCOVER, IP_RECVTOS})`
    /// fails with `errno_for_options` and `recvmmsg` fails with `ENOSYS`.
    /// The filter dies with the test thread.
    fn install_shadow_behavior_filter(errno_for_options: i32) {
        // Classic BPF over `struct seccomp_data`.
        const BPF_LD_W_ABS: u16 = 0x20;
        const BPF_JMP_JEQ_K: u16 = 0x15;
        const BPF_RET_K: u16 = 0x06;
        const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
        const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
        // seccomp_data offsets: nr at 0; args[N] at 16 + 8*N (low word first;
        // both CI architectures are little-endian).
        const OFF_NR: u32 = 0;
        const OFF_ARG1_LO: u32 = 16 + 8; // `level`
        const OFF_ARG2_LO: u32 = 16 + 16; // `optname`

        const fn stmt(code: u16, k: u32) -> libc::sock_filter {
            libc::sock_filter {
                code,
                jt: 0,
                jf: 0,
                k,
            }
        }
        const fn jeq(k: u32, jt: u8, jf: u8) -> libc::sock_filter {
            libc::sock_filter {
                code: BPF_JMP_JEQ_K,
                jt,
                jf,
                k,
            }
        }

        let mut filter = [
            // 0: if (nr == recvmmsg) goto ENOSYS(10)
            stmt(BPF_LD_W_ABS, OFF_NR),
            jeq(libc::SYS_recvmmsg as u32, 8, 0),
            // 2: if (nr != setsockopt) goto ALLOW(11)
            jeq(libc::SYS_setsockopt as u32, 0, 8),
            // 3: if (level != IPPROTO_IP) goto ALLOW(11)
            stmt(BPF_LD_W_ABS, OFF_ARG1_LO),
            jeq(libc::IPPROTO_IP as u32, 0, 6),
            // 5: if (optname in {IP_PKTINFO, IP_MTU_DISCOVER, IP_RECVTOS})
            //    goto ERRNO(9)
            stmt(BPF_LD_W_ABS, OFF_ARG2_LO),
            jeq(libc::IP_PKTINFO as u32, 2, 0),
            jeq(libc::IP_MTU_DISCOVER as u32, 1, 0),
            jeq(libc::IP_RECVTOS as u32, 0, 2),
            // 9: option ERRNO; 10: ENOSYS; 11: ALLOW
            stmt(BPF_RET_K, SECCOMP_RET_ERRNO | errno_for_options as u32),
            stmt(BPF_RET_K, SECCOMP_RET_ERRNO | libc::ENOSYS as u32),
            stmt(BPF_RET_K, SECCOMP_RET_ALLOW),
        ];
        let prog = libc::sock_fprog {
            len: filter.len() as u16,
            filter: filter.as_mut_ptr(),
        };

        unsafe {
            assert_eq!(
                libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0),
                0,
                "PR_SET_NO_NEW_PRIVS failed: {}",
                std::io::Error::last_os_error()
            );
            assert_eq!(
                libc::prctl(
                    libc::PR_SET_SECCOMP,
                    libc::SECCOMP_MODE_FILTER,
                    &prog as *const libc::sock_fprog,
                ),
                0,
                "installing the seccomp filter failed: {}",
                std::io::Error::last_os_error()
            );
        }
    }

    /// Receive one datagram through the `recvmmsg` override into `buf`,
    /// returning its length.
    fn recv_one(sock: &UdpSocket, buf: &mut [u8]) -> usize {
        let mut name = MaybeUninit::<libc::sockaddr_storage>::uninit();
        let mut iov = [IoSliceMut::new(buf)];
        let mut msgvec: libc::mmsghdr = unsafe { std::mem::zeroed() };
        msgvec.msg_hdr.msg_name = name.as_mut_ptr() as *mut _;
        msgvec.msg_hdr.msg_namelen = std::mem::size_of::<libc::sockaddr_storage>() as _;
        msgvec.msg_hdr.msg_iov = iov.as_mut_ptr() as *mut libc::iovec;
        msgvec.msg_hdr.msg_iovlen = 1;
        let n = unsafe { recvmmsg(sock.as_raw_fd(), &mut msgvec, 1, 0, std::ptr::null_mut()) };
        assert_eq!(n, 1, "recvmmsg failed: {}", std::io::Error::last_os_error());
        msgvec.msg_len as usize
    }

    /// Set one socket option through the override, returning the raw result.
    fn set_opt(sock: &UdpSocket, level: c_int, optname: c_int) -> c_int {
        let on: c_int = 1;
        unsafe {
            setsockopt(
                sock.as_raw_fd(),
                level,
                optname,
                &on as *const _ as *const c_void,
                std::mem::size_of::<c_int>() as libc::socklen_t,
            )
        }
    }

    /// On a real kernel every forwarded call succeeds: the overrides are pure
    /// pass-through and the stubs never engage. (`ENOSYS_SEEN` is process
    /// state, so if the sibling test ran first the receive below goes through
    /// the emulation instead — the functional assertions hold either way.)
    #[test]
    fn passes_through_on_a_real_kernel() {
        let recv = UdpSocket::bind("127.0.0.1:0").unwrap();
        assert_eq!(set_opt(&recv, libc::IPPROTO_IP, libc::IP_PKTINFO), 0);
        assert_eq!(set_opt(&recv, libc::IPPROTO_IP, libc::IP_RECVTOS), 0);

        let send = UdpSocket::bind("127.0.0.1:0").unwrap();
        send.send_to(b"hello", recv.local_addr().unwrap()).unwrap();
        let mut buf = [0u8; 64];
        let len = recv_one(&recv, &mut buf);
        assert_eq!(&buf[..len], b"hello");
    }

    /// Under Shadow's behavior (targeted options fail `ENOPROTOOPT`,
    /// `recvmmsg` fails `ENOSYS`) the stubs engage: the options report
    /// success and receives are emulated through `recvmsg`.
    #[test]
    fn stubs_engage_under_shadow_behavior() {
        let recv = UdpSocket::bind("127.0.0.1:0").unwrap();
        let send = UdpSocket::bind("127.0.0.1:0").unwrap();
        install_shadow_behavior_filter(libc::ENOPROTOOPT);

        // The kernel-level rejection is stubbed to success…
        assert_eq!(set_opt(&recv, libc::IPPROTO_IP, libc::IP_PKTINFO), 0);
        assert_eq!(set_opt(&recv, libc::IPPROTO_IP, libc::IP_MTU_DISCOVER), 0);
        assert_eq!(set_opt(&recv, libc::IPPROTO_IP, libc::IP_RECVTOS), 0);
        // …while an untargeted option is forwarded untouched.
        assert_eq!(set_opt(&recv, libc::SOL_SOCKET, libc::SO_REUSEADDR), 0);

        // recvmmsg observes ENOSYS once, then delivers via the recvmsg
        // emulation.
        send.send_to(b"hello", recv.local_addr().unwrap()).unwrap();
        let mut buf = [0u8; 64];
        let len = recv_one(&recv, &mut buf);
        assert_eq!(&buf[..len], b"hello");
        // The emulation keeps working on subsequent receives.
        send.send_to(b"again", recv.local_addr().unwrap()).unwrap();
        let len = recv_one(&recv, &mut buf);
        assert_eq!(&buf[..len], b"again");
    }
}
