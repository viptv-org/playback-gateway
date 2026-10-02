use std::path::Path;

pub fn isolated_address() {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    assert_eq!(
        std::env::var("PLAYBACK_TEST_ISOLATED_NETWORK").as_deref(),
        Ok("container")
    );
    assert!(
        Path::new("/.dockerenv").exists(),
        "never configure host networking"
    );
    let interfaces: Vec<_> = std::fs::read_dir("/sys/class/net")
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(
        interfaces,
        vec![std::ffi::OsString::from("lo")],
        "run with Docker network none"
    );
    // Same guarded public-classified loopback seam as the proven backend fixture.
    // These ioctls can affect only this disposable, network-none namespace.
    unsafe {
        let raw = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
        assert!(raw >= 0);
        let socket = OwnedFd::from_raw_fd(raw);
        let mut request: libc::ifreq = std::mem::zeroed();
        for (slot, byte) in request.ifr_name.iter_mut().zip(b"lo:fixture\0") {
            *slot = *byte as libc::c_char;
        }
        for (operation, octets) in [
            (libc::SIOCSIFADDR, [11, 255, 255, 1]),
            (libc::SIOCSIFNETMASK, [255, 255, 255, 255]),
        ] {
            let address = libc::sockaddr_in {
                sin_family: libc::AF_INET as u16,
                sin_port: 0,
                sin_addr: libc::in_addr {
                    s_addr: u32::from_ne_bytes(octets),
                },
                sin_zero: [0; 8],
            };
            request.ifr_ifru.ifru_addr =
                std::ptr::read((&address as *const libc::sockaddr_in).cast::<libc::sockaddr>());
            assert_eq!(
                libc::ioctl(socket.as_raw_fd(), operation, &request),
                0,
                "fixture requires NET_ADMIN"
            );
        }
    }
}
