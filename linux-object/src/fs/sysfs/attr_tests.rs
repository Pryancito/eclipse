//! The attributes of `/sys` that can be written: the thermal zone and its
//! cooling device, `/sys/power` and each CPU's `online`. What they read
//! back, and what they refuse, against `drivers/thermal/thermal_sysfs.c`,
//! `kernel/power/main.c` and `drivers/base/cpu.c`.

use super::tree_tests::contents;
use super::*;

fn at(path: &str) -> Arc<dyn INode> {
    SYS_ROOT.lookup_follow(path, 4).unwrap()
}

fn read(path: &str) -> String {
    String::from_utf8(contents(&at(path))).unwrap()
}

/// What is written is read back from a node opened afresh: a `find` makes
/// a new inode each time, and the value is the zone's, not the inode's.
#[test]
fn a_thermal_attribute_keeps_what_was_written() {
    let _bus = test_bus::with(test_bus::a_real_machine());
    let zone = "class/thermal/thermal_zone0";
    let before = read(&format!("{}/trip_point_0_temp", zone));
    let node = at(&format!("{}/trip_point_0_temp", zone));
    assert_eq!(node.write_at(0, b"85000\n"), Ok(6), "all of it taken");
    assert_eq!(read(&format!("{}/trip_point_0_temp", zone)), "85000\n");
    assert_eq!(
        node.metadata().unwrap().size,
        6,
        "its size is what it reads"
    );
    assert_eq!(node.metadata().unwrap().mode, 0o644);
    // A temperature that is not a number is EINVAL and changes nothing.
    assert!(node.write_at(0, b"hot\n").is_err());
    assert!(node.write_at(0, &[0xff, 0xfe]).is_err(), "not text");
    assert_eq!(read(&format!("{}/trip_point_0_temp", zone)), "85000\n");
    node.write_at(0, before.as_bytes()).unwrap();

    let policy = at(&format!("{}/policy", zone));
    let was = read(&format!("{}/policy", zone));
    assert!(read(&format!("{}/available_policies", zone))
        .split_whitespace()
        .any(|p| p == was.trim()));
    policy.write_at(0, b"user_space\n").unwrap();
    assert_eq!(read(&format!("{}/policy", zone)), "user_space\n");
    policy.write_at(0, was.as_bytes()).unwrap();
    assert!(policy.poll().unwrap().write);
}

/// A thermal zone says its temperature in millidegrees Celsius, of a kind
/// and with trip points of a type `thermal_sysfs.c` knows.
#[test]
fn a_thermal_zone_reads_as_linux_writes_one() {
    let _bus = test_bus::with(test_bus::a_real_machine());
    let zone = "class/thermal/thermal_zone0";
    let temp: i32 = read(&format!("{}/temp", zone)).trim().parse().unwrap();
    assert!((0..150_000).contains(&temp), "{} m°C", temp);
    assert_eq!(read(&format!("{}/mode", zone)), "enabled\n");
    for trip in ["trip_point_0_type", "trip_point_1_type"] {
        let t = read(&format!("{}/{}", zone, trip));
        assert!(
            ["active\n", "passive\n", "hot\n", "critical\n"].contains(&t.as_str()),
            "{}: {:?}",
            trip,
            t
        );
    }
    assert!(!read(&format!("{}/type", zone)).trim().is_empty());
}

/// `cur_state` of a cooling device is at most its `max_state`.
#[test]
fn a_cooling_device_never_goes_past_its_max_state() {
    let _bus = test_bus::with(test_bus::a_real_machine());
    let dev = "class/thermal/cooling_device0";
    let max: u32 = read(&format!("{}/max_state", dev)).trim().parse().unwrap();
    let cur = at(&format!("{}/cur_state", dev));
    let was = read(&format!("{}/cur_state", dev));
    cur.write_at(0, b"1\n").unwrap();
    assert_eq!(read(&format!("{}/cur_state", dev)), "1\n");
    cur.write_at(0, format!("{}\n", max + 50).as_bytes()).ok();
    let now: u32 = read(&format!("{}/cur_state", dev)).trim().parse().unwrap();
    assert!(now <= max, "{} past {}", now, max);
    assert!(cur.write_at(0, b"-1\n").is_err());
    cur.write_at(0, was.as_bytes()).unwrap();
    assert_eq!(read(&format!("{}/type", dev)), "Processor\n");
}

/// `/sys/power/state` lists the states a write may name, one line, and a
/// state it does not list is `EINVAL`.
#[test]
fn power_state_refuses_a_state_it_does_not_list() {
    let _bus = test_bus::with(test_bus::a_real_machine());
    let listed = read("power/state");
    assert_eq!(listed.matches('\n').count(), 1);
    let state = at("power/state");
    assert!(state.write_at(0, b"hibernate-now\n").is_err());
    assert!(state.write_at(0, b"\n").is_err());
    // Nothing here sleeps, so nothing is listed, and "mem" (what a power
    // manager writes to suspend) is EINVAL rather than a lie.
    assert_eq!(listed, "\n");
    for st in [&b"mem\n"[..], b"freeze", b"disk", b"standby"] {
        assert_eq!(state.write_at(0, st), Err(FsError::InvalidParam));
    }
    assert_eq!(read("power/state"), listed, "unchanged");
    assert_eq!(read("power/wakeup_count"), "0\n");
}

/// `/sys/power/disk` takes the modes it knows, keeps the one taken, and
/// refuses the rest.
#[test]
fn power_disk_takes_a_mode_it_knows() {
    let _bus = test_bus::with(test_bus::a_real_machine());
    let disk = at("power/disk");
    let was = read("power/disk");
    assert_eq!(disk.write_at(0, b"reboot\n"), Ok(7));
    assert!(read("power/disk").contains("reboot"));
    assert!(disk.write_at(0, b"explode\n").is_err());
    assert!(read("power/disk").contains("reboot"), "unchanged");
    disk.write_at(0, was.trim().as_bytes()).unwrap();
}

/// `/sys/devices/system/cpu`: `present` and `possible` are the range of
/// every CPU, `online` the ones that are, and there is one `cpuN` per CPU
/// and no more.
#[test]
fn the_cpus_are_listed_as_ranges_and_one_directory_each() {
    let _bus = test_bus::with(test_bus::a_real_machine());
    let count = kernel_hal::cpu::cpu_count() as usize;
    let range = if count == 1 {
        String::from("0\n")
    } else {
        format!("0-{}\n", count - 1)
    };
    assert_eq!(read("devices/system/cpu/present"), range);
    assert_eq!(read("devices/system/cpu/possible"), range);
    assert_eq!(read("devices/system/cpu/online"), range);
    // `kernel_max` is `NR_CPUS - 1`, what the kernel was built for, not
    // the CPUs there are.
    assert_eq!(
        read("devices/system/cpu/kernel_max"),
        format!("{}\n", kernel_hal::config::MAX_CORE_NUM - 1)
    );
    let cpu = at("devices/system/cpu");
    for i in 0..count {
        let n = cpu.find(&format!("cpu{}", i)).unwrap();
        assert_eq!(n.metadata().unwrap().type_, FileType::Dir);
        assert_eq!(contents(&n.find("online").unwrap()), b"1\n");
    }
    assert!(cpu.find(&format!("cpu{}", count)).is_err());
    assert!(cpu.find("cpu").is_err());
    assert!(cpu.find("cpux").is_err());
}

/// The boot CPU cannot be taken offline (`cpu_down` of the last one, and
/// on x86 cpu0 has no `online` at all); a value that is not a number is
/// `EINVAL`.
#[test]
fn the_boot_cpu_stays_online() {
    let _bus = test_bus::with(test_bus::a_real_machine());
    let online = at("devices/system/cpu/cpu0/online");
    assert!(online.write_at(0, b"0\n").is_err());
    assert!(online.write_at(0, b"maybe\n").is_err());
    assert_eq!(online.write_at(0, b"1\n"), Ok(2));
    assert_eq!(contents(&online), b"1\n");
}
