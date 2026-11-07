//! Is the Mac reaching the speaker over Wi-Fi or a cable? Over Wi-Fi the stream
//! crosses the air twice and stalls of 1-2 s happen, so the speaker needs a
//! larger buffer margin (measured 2026-10-11).

use std::ffi::CStr;
use std::net::IpAddr;
use std::process::Command;

/// The name of the local interface that has `ip`, e.g. "en0".
pub fn interface_with(ip: IpAddr) -> Option<String> {
    let IpAddr::V4(ip) = ip else { return None };
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        return None;
    }
    let mut found = None;
    let mut cursor = list;
    while !cursor.is_null() {
        let entry = unsafe { &*cursor };
        if !entry.ifa_addr.is_null() && unsafe { (*entry.ifa_addr).sa_family } as i32 == libc::AF_INET {
            let addr = unsafe { &*(entry.ifa_addr as *const libc::sockaddr_in) };
            if u32::from_be(addr.sin_addr.s_addr) == u32::from(ip) {
                found = Some(unsafe { CStr::from_ptr(entry.ifa_name) }.to_string_lossy().into_owned());
                break;
            }
        }
        cursor = entry.ifa_next;
    }
    unsafe { libc::freeifaddrs(list) };
    found
}

/// Interfaces macOS lists as Wi-Fi hardware ports.
pub fn wifi_interfaces() -> Vec<String> {
    let Ok(out) = Command::new("/usr/sbin/networksetup").arg("-listallhardwareports").output() else {
        return Vec::new();
    };
    parse_wifi_ports(&String::from_utf8_lossy(&out.stdout))
}

fn parse_wifi_ports(listing: &str) -> Vec<String> {
    let mut wifi = Vec::new();
    let mut in_wifi = false;
    for line in listing.lines() {
        if let Some(port) = line.strip_prefix("Hardware Port: ") {
            in_wifi = port.trim() == "Wi-Fi" || port.trim() == "AirPort";
        } else if let Some(device) = line.strip_prefix("Device: ") {
            if in_wifi {
                wifi.push(device.trim().to_string());
            }
            in_wifi = false;
        }
    }
    wifi
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_wifi_devices() {
        let listing = "\nHardware Port: Ethernet\nDevice: en18\nEthernet Address: aa\n\n\
                       Hardware Port: Wi-Fi\nDevice: en0\nEthernet Address: bb\n\n\
                       Hardware Port: Thunderbolt Bridge\nDevice: bridge0\n";
        assert_eq!(parse_wifi_ports(listing), ["en0"]);
    }

    #[test]
    fn finds_the_loopback_interface() {
        assert_eq!(interface_with("127.0.0.1".parse().unwrap()).as_deref(), Some("lo0"));
    }
}
