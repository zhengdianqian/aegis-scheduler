// maskbench: 测量进程内（无 exec 开销）umount2 + mount --bind 三条 mask 节点的耗时。
use std::ffi::CString;
use std::time::Instant;

const NODES: [&str; 3] = [
    "/sys/devices/system/cpu/cpufreq/policy0/scaling_min_freq",
    "/sys/devices/system/cpu/cpufreq/policy3/scaling_min_freq",
    "/sys/devices/system/cpu/cpufreq/policy7/scaling_min_freq",
];
const MASKS: [&str; 3] = [
    "/data/adb/tempo/mask/policy0",
    "/data/adb/tempo/mask/policy3",
    "/data/adb/tempo/mask/policy7",
];

fn c(s: &str) -> CString {
    CString::new(s).unwrap()
}

fn main() {
    let nodes: Vec<CString> = NODES.iter().map(|s| c(s)).collect();
    let masks: Vec<CString> = MASKS.iter().map(|s| c(s)).collect();

    for round in 0..20 {
        let t0 = Instant::now();
        for n in &nodes {
            unsafe {
                libc::umount2(n.as_ptr(), 0);
            }
        }
        let t1 = Instant::now();
        for (m, n) in masks.iter().zip(nodes.iter()) {
            unsafe {
                libc::mount(
                    m.as_ptr(),
                    n.as_ptr(),
                    std::ptr::null(),
                    libc::MS_BIND as libc::c_ulong,
                    std::ptr::null(),
                );
            }
        }
        let t2 = Instant::now();
        println!(
            "CYCLE {} umount_us={} mount_us={} total_us={}",
            round,
            (t1 - t0).as_micros(),
            (t2 - t1).as_micros(),
            (t2 - t0).as_micros()
        );
    }
}
