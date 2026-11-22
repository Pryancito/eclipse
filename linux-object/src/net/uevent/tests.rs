use super::*;
use core::convert::TryInto;

/// The strings after the header (or the `ACTION@DEVPATH` line).
fn strings(body: &[u8]) -> Vec<String> {
    body.split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8(s.to_vec()).unwrap())
        .collect()
}

fn ev<'a>(devpath: &'a str, props: &'a [(&'a str, &'a str)]) -> Uevent<'a> {
    Uevent {
        action: "change",
        devpath,
        props,
    }
}

/// What udevd puts on group 2, field by field as libudev's
/// `device_monitor_receive_device` checks it: the "libudev" prefix, the
/// magic in network order, properties at the offset the header says and at
/// least 32 bytes past it, and ACTION, DEVPATH, SUBSYSTEM and SEQNUM there.
#[test]
fn the_udev_form_is_what_libudev_accepts() {
    let props = [("SUBSYSTEM", "drm"), ("DEVNAME", "/dev/dri/card1")];
    let m = udev_message(&ev("/devices/virtual/drm/card1", &props), 7);
    assert_eq!(&m[..8], b"libudev\0");
    assert_eq!(
        u32::from_be_bytes(m[8..12].try_into().unwrap()),
        0xfeed_cafe
    );
    let off = u32::from_ne_bytes(m[16..20].try_into().unwrap()) as usize;
    let len = u32::from_ne_bytes(m[20..24].try_into().unwrap()) as usize;
    assert_eq!(off, 40);
    assert_eq!(off + len, m.len());
    assert!(off + 32 <= m.len());
    assert_eq!(
        strings(&m[off..]),
        [
            "ACTION=change",
            "DEVPATH=/devices/virtual/drm/card1",
            "SUBSYSTEM=drm",
            "DEVNAME=/dev/dri/card1",
            "SEQNUM=7",
        ]
    );
}

/// The kernel's own form, as `kobject_uevent_env` sends it: the
/// `ACTION@DEVPATH` line first, `DEVNAME` relative to /dev.
#[test]
fn the_kernel_form_leads_with_action_at_devpath() {
    let props = [("SUBSYSTEM", "drm"), ("DEVNAME", "/dev/dri/card0")];
    let m = kernel_message(&ev("/devices/virtual/drm/card0", &props), 3);
    let s = strings(&m);
    assert_eq!(s[0], "change@/devices/virtual/drm/card0");
    assert!(s.contains(&String::from("DEVNAME=dri/card0")));
    assert!(s.contains(&String::from("SEQNUM=3")));
}

fn mine(sock: &UeventSocket, devpath: &str) -> Vec<(Vec<u8>, u32)> {
    let mut found = Vec::new();
    let mut buf = [0u8; 2048];
    while let Some((n, group)) = sock.take(&mut buf, true) {
        if buf[..n]
            .windows(devpath.len())
            .any(|w| w == devpath.as_bytes())
        {
            found.push((buf[..n].to_vec(), group));
        }
    }
    found
}

/// A socket bound to the udev group gets the udev form and only that; one
/// bound to no group gets nothing; and each message comes with root's
/// credentials once SO_PASSCRED is on, from pid 0 and its group's mask.
#[test]
fn a_hotplug_reaches_the_sockets_bound_to_its_group() {
    let udev = UeventSocket::new(SocketType::SOCK_RAW);
    udev.bind(Endpoint::Netlink(NetlinkEndpoint::new(
        0,
        1 << (GROUP_UDEV - 1),
    )))
    .unwrap();
    let both = UeventSocket::new(SocketType::SOCK_RAW);
    both.bind(Endpoint::Netlink(NetlinkEndpoint::new(0, 0b11)))
        .unwrap();
    let none = UeventSocket::new(SocketType::SOCK_RAW);
    assert!(!udev.readable());

    let devpath = "/devices/virtual/drm/card9";
    broadcast(&ev(devpath, &[("SUBSYSTEM", "drm")]));

    let got = mine(&udev, devpath);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].1, GROUP_UDEV);
    assert_eq!(&got[0].0[..8], b"libudev\0");
    let groups: Vec<u32> = mine(&both, devpath).iter().map(|(_, g)| *g).collect();
    assert_eq!(groups, [GROUP_KERNEL, GROUP_UDEV]);
    assert!(mine(&none, devpath).is_empty());

    assert_eq!(udev.recv_creds(), None, "no credentials until asked");
    udev.set_passcred(true).unwrap();
    assert_eq!(udev.recv_creds(), Some([0; 12]));
    assert_eq!(Socket::write(&udev, b"x", None), Err(LxError::EPERM));
}

/// The DRM hotplug udevd would pass on: card, devnode and devnum where
/// wlroots looks for them (`sysname`, `DEVNAME`, `MAJOR`/`MINOR`).
#[test]
fn a_drm_hotplug_names_the_card_wlroots_opened() {
    let sock = UeventSocket::new(SocketType::SOCK_RAW);
    sock.bind(Endpoint::Netlink(NetlinkEndpoint::new(0, 2)))
        .unwrap();
    drm_hotplug(7, Some((0x65, 0, 0)));
    let devpath = "/devices/pci0000:00/0000:65:00.0/drm/card7";
    let got = mine(&sock, devpath);
    assert_eq!(got.len(), 1);
    let s = strings(&got[0].0[40..]);
    for want in [
        "ACTION=change",
        "SUBSYSTEM=drm",
        "HOTPLUG=1",
        "DEVNAME=/dev/dri/card7",
        "MAJOR=226",
        "MINOR=7",
    ] {
        assert!(s.contains(&String::from(want)), "{} missing: {:?}", want, s);
    }
}
