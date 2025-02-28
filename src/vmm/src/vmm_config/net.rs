// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::convert::TryInto;
use std::ops::Deref;
use std::sync::{Arc, Mutex};
use std::{fmt, result};

use devices::virtio::net::TapError;
use devices::virtio::Net;
use serde::{Deserialize, Serialize};
use utils::net::mac::MacAddr;

use super::RateLimiterConfig;
use crate::Error as VmmError;

/// This struct represents the strongly typed equivalent of the json body from net iface
/// related requests.
#[derive(Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkInterfaceConfig {
    /// ID of the guest network interface.
    pub iface_id: String,
    /// Host level path for the guest network interface.
    pub host_dev_name: String,
    /// Guest MAC address.
    pub guest_mac: Option<MacAddr>,
    /// Rate Limiter for received packages.
    pub rx_rate_limiter: Option<RateLimiterConfig>,
    /// Rate Limiter for transmitted packages.
    pub tx_rate_limiter: Option<RateLimiterConfig>,
}

impl From<&Net> for NetworkInterfaceConfig {
    fn from(net: &Net) -> Self {
        let rx_rl: RateLimiterConfig = net.rx_rate_limiter().into();
        let tx_rl: RateLimiterConfig = net.tx_rate_limiter().into();
        NetworkInterfaceConfig {
            iface_id: net.id().clone(),
            host_dev_name: net.iface_name(),
            guest_mac: net.guest_mac().copied(),
            rx_rate_limiter: rx_rl.into_option(),
            tx_rate_limiter: tx_rl.into_option(),
        }
    }
}

/// The data fed into a network iface update request. Currently, only the RX and TX rate limiters
/// can be updated.
#[derive(Debug, Deserialize, PartialEq, Clone)]
#[serde(deny_unknown_fields)]
pub struct NetworkInterfaceUpdateConfig {
    /// The net iface ID, as provided by the user at iface creation time.
    pub iface_id: String,
    /// New RX rate limiter config. Only provided data will be updated. I.e. if any optional data
    /// is missing, it will not be nullified, but left unchanged.
    pub rx_rate_limiter: Option<RateLimiterConfig>,
    /// New TX rate limiter config. Only provided data will be updated. I.e. if any optional data
    /// is missing, it will not be nullified, but left unchanged.
    pub tx_rate_limiter: Option<RateLimiterConfig>,
}

/// Errors associated with `NetworkInterfaceConfig`.
#[derive(Debug)]
pub enum NetworkInterfaceError {
    /// Could not create Network Device.
    CreateNetworkDevice(devices::virtio::net::Error),
    /// Failed to create a `RateLimiter` object.
    CreateRateLimiter(std::io::Error),
    /// The MAC address is already in use.
    GuestMacAddressInUse(String),
    /// Error during interface update (patch).
    DeviceUpdate(VmmError),
    /// Cannot open/create tap device.
    OpenTap(TapError),
}

impl fmt::Display for NetworkInterfaceError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::NetworkInterfaceError::*;
        match self {
            CreateNetworkDevice(e) => write!(f, "Could not create Network Device: {:?}", e),
            CreateRateLimiter(e) => write!(f, "Cannot create RateLimiter: {}", e),
            GuestMacAddressInUse(mac_addr) => write!(
                f,
                "{}",
                format!("The guest MAC address {} is already in use.", mac_addr)
            ),
            DeviceUpdate(e) => write!(f, "Error during interface update (patch): {}", e),
            OpenTap(e) => {
                // We are propagating the Tap Error. This error can contain
                // imbricated quotes which would result in an invalid json.
                let mut tap_err = format!("{:?}", e);
                tap_err = tap_err.replace("\"", "");

                write!(
                    f,
                    "{}{}",
                    "Cannot open TAP device. Invalid name/permissions. ".to_string(),
                    tap_err
                )
            }
        }
    }
}

type Result<T> = result::Result<T, NetworkInterfaceError>;

/// Builder for a list of network devices.
#[derive(Default)]
pub struct NetBuilder {
    net_devices: Vec<Arc<Mutex<Net>>>,
}

impl NetBuilder {
    /// Creates an empty list of Network Devices.
    pub fn new() -> Self {
        NetBuilder {
            /// List of built network devices.
            net_devices: Vec::new(),
        }
    }

    /// Returns a immutable iterator over the network devices.
    pub fn iter(&self) -> ::std::slice::Iter<Arc<Mutex<Net>>> {
        self.net_devices.iter()
    }

    /// Returns a mutable iterator over the network devices.
    pub fn iter_mut(&mut self) -> ::std::slice::IterMut<Arc<Mutex<Net>>> {
        self.net_devices.iter_mut()
    }

    /// Adds an existing network device in the builder.
    pub fn add_device(&mut self, device: Arc<Mutex<Net>>) {
        self.net_devices.push(device);
    }

    /// Builds a network device based on a network interface config. Keeps a device reference
    /// in the builder's internal list.
    pub fn build(&mut self, netif_config: NetworkInterfaceConfig) -> Result<Arc<Mutex<Net>>> {
        let mac_conflict = |net: &Arc<Mutex<Net>>| {
            let net = net.lock().expect("Poisoned lock");
            // Check if another net dev has same MAC.
            netif_config.guest_mac.is_some()
                && netif_config.guest_mac.as_ref() == net.guest_mac()
                && &netif_config.iface_id != net.id()
        };
        // Validate there is no Mac conflict.
        // No need to validate host_dev_name conflict. In such a case,
        // an error will be thrown during device creation anyway.
        if self.net_devices.iter().any(mac_conflict) {
            return Err(NetworkInterfaceError::GuestMacAddressInUse(
                netif_config.guest_mac.unwrap().to_string(),
            ));
        }

        // If this is an update, just remove the old one.
        if let Some(index) = self
            .net_devices
            .iter()
            .position(|net| net.lock().expect("Poisoned lock").id() == &netif_config.iface_id)
        {
            self.net_devices.swap_remove(index);
        }

        // Add new device.
        let net = Arc::new(Mutex::new(Self::create_net(netif_config)?));
        self.net_devices.push(net.clone());

        Ok(net)
    }

    /// Creates a Net device from a NetworkInterfaceConfig.
    pub fn create_net(cfg: NetworkInterfaceConfig) -> Result<Net> {
        let rx_rate_limiter = cfg
            .rx_rate_limiter
            .map(super::RateLimiterConfig::try_into)
            .transpose()
            .map_err(NetworkInterfaceError::CreateRateLimiter)?;
        let tx_rate_limiter = cfg
            .tx_rate_limiter
            .map(super::RateLimiterConfig::try_into)
            .transpose()
            .map_err(NetworkInterfaceError::CreateRateLimiter)?;

        // Create and return the Net device
        devices::virtio::net::Net::new_with_tap(
            cfg.iface_id,
            cfg.host_dev_name.clone(),
            cfg.guest_mac.as_ref(),
            rx_rate_limiter.unwrap_or_default(),
            tx_rate_limiter.unwrap_or_default(),
        )
        .map_err(NetworkInterfaceError::CreateNetworkDevice)
    }

    /// Returns a vec with the structures used to configure the net devices.
    pub fn configs(&self) -> Vec<NetworkInterfaceConfig> {
        let mut ret = vec![];
        for net in &self.net_devices {
            ret.push(NetworkInterfaceConfig::from(net.lock().unwrap().deref()));
        }
        ret
    }
}

#[cfg(test)]
mod tests {
    use std::str;

    use rate_limiter::RateLimiter;

    use super::*;

    impl NetBuilder {
        pub fn len(&self) -> usize {
            self.net_devices.len()
        }

        pub fn is_empty(&self) -> bool {
            self.net_devices.len() == 0
        }
    }

    fn create_netif(id: &str, name: &str, mac: &str) -> NetworkInterfaceConfig {
        NetworkInterfaceConfig {
            iface_id: String::from(id),
            host_dev_name: String::from(name),
            guest_mac: Some(MacAddr::parse_str(mac).unwrap()),
            rx_rate_limiter: RateLimiterConfig::default().into_option(),
            tx_rate_limiter: RateLimiterConfig::default().into_option(),
        }
    }

    impl Clone for NetworkInterfaceConfig {
        fn clone(&self) -> Self {
            NetworkInterfaceConfig {
                iface_id: self.iface_id.clone(),
                host_dev_name: self.host_dev_name.clone(),
                guest_mac: self.guest_mac,
                rx_rate_limiter: None,
                tx_rate_limiter: None,
            }
        }
    }

    #[test]
    fn test_insert() {
        let mut net_builder = NetBuilder::new();

        let id_1 = "id_1";
        let mut host_dev_name_1 = "dev1";
        let mut guest_mac_1 = "01:23:45:67:89:0a";

        // Test create.
        let netif_1 = create_netif(id_1, host_dev_name_1, guest_mac_1);
        assert!(net_builder.build(netif_1).is_ok());
        assert_eq!(net_builder.net_devices.len(), 1);

        // Test update mac address (this test does not modify the tap).
        guest_mac_1 = "01:23:45:67:89:0b";
        let netif_1 = create_netif(id_1, host_dev_name_1, guest_mac_1);

        assert!(net_builder.build(netif_1).is_ok());
        assert_eq!(net_builder.net_devices.len(), 1);

        // Test update host_dev_name (the tap will be updated).
        host_dev_name_1 = "dev2";
        let netif_1 = create_netif(id_1, host_dev_name_1, guest_mac_1);
        assert!(net_builder.build(netif_1).is_ok());
        assert_eq!(net_builder.net_devices.len(), 1);
    }

    #[test]
    fn test_insert_error_cases() {
        let mut net_builder = NetBuilder::new();

        let id_1 = "id_1";
        let host_dev_name_1 = "dev3";
        let guest_mac_1 = "01:23:45:67:89:0a";

        // Adding the first valid network config.
        let netif_1 = create_netif(id_1, host_dev_name_1, guest_mac_1);
        assert!(net_builder.build(netif_1).is_ok());

        // Error Cases for CREATE
        // Error Case: Add new network config with the same mac as netif_1.
        let id_2 = "id_2";
        let host_dev_name_2 = "dev4";
        let guest_mac_2 = "01:23:45:67:89:0b";

        let netif_2 = create_netif(id_2, host_dev_name_2, guest_mac_1);
        let expected_error = format!(
            "The guest MAC address {} is already in use.",
            guest_mac_1.to_string()
        );
        assert_eq!(
            net_builder.build(netif_2).err().unwrap().to_string(),
            expected_error
        );
        assert_eq!(net_builder.net_devices.len(), 1);

        // Error Case: Add new network config with the same dev_host_name as netif_1.
        let netif_2 = create_netif(id_2, host_dev_name_1, guest_mac_2);
        assert_eq!(
            net_builder.build(netif_2).err().unwrap().to_string(),
            NetworkInterfaceError::CreateNetworkDevice(devices::virtio::net::Error::TapOpen(
                TapError::IoctlError(std::io::Error::from_raw_os_error(16))
            ))
            .to_string()
        );
        assert_eq!(net_builder.net_devices.len(), 1);

        // Adding the second valid network config.
        let netif_2 = create_netif(id_2, host_dev_name_2, guest_mac_2);
        assert!(net_builder.build(netif_2).is_ok());

        // Error Cases for UPDATE
        // Error Case: Update netif_2 mac using the same mac as netif_1.
        let netif_2 = create_netif(id_2, host_dev_name_2, guest_mac_1);
        let expected_error = format!(
            "The guest MAC address {} is already in use.",
            guest_mac_1.to_string()
        );
        assert_eq!(
            net_builder.build(netif_2).err().unwrap().to_string(),
            expected_error
        );

        // Error Case: Update netif_2 dev_host_name using the same dev_host_name as netif_1.
        let netif_2 = create_netif(id_2, host_dev_name_1, guest_mac_2);
        assert_eq!(
            net_builder.build(netif_2).err().unwrap().to_string(),
            NetworkInterfaceError::CreateNetworkDevice(devices::virtio::net::Error::TapOpen(
                TapError::IoctlError(std::io::Error::from_raw_os_error(16))
            ))
            .to_string()
        );
    }

    #[test]
    fn test_error_display() {
        // FIXME: use macro
        let err = NetworkInterfaceError::CreateNetworkDevice(devices::virtio::net::Error::TapOpen(
            TapError::InvalidIfname,
        ));
        let _ = format!("{}{:?}", err, err);
        let err = NetworkInterfaceError::CreateRateLimiter(std::io::Error::from_raw_os_error(0));
        let _ = format!("{}{:?}", err, err);
        let _ = format!(
            "{}{:?}",
            NetworkInterfaceError::DeviceUpdate(VmmError::VcpuExit),
            NetworkInterfaceError::DeviceUpdate(VmmError::VcpuExit)
        );
        let _ = format!(
            "{}{:?}",
            NetworkInterfaceError::OpenTap(TapError::InvalidIfname),
            NetworkInterfaceError::OpenTap(TapError::InvalidIfname)
        );
    }

    #[test]
    fn test_net_config() {
        let net_id = "id";
        let host_dev_name = "dev";
        let guest_mac = "01:23:45:67:89:0b";

        let net_if_cfg = create_netif(net_id, host_dev_name, guest_mac);
        assert_eq!(
            net_if_cfg.guest_mac.unwrap(),
            MacAddr::parse_str(guest_mac).unwrap()
        );

        let mut net_builder = NetBuilder::new();
        assert!(net_builder.build(net_if_cfg.clone()).is_ok());
        assert_eq!(net_builder.net_devices.len(), 1);

        let configs = net_builder.configs();
        assert_eq!(configs.len(), 1);
        assert_eq!(configs.first().unwrap(), &net_if_cfg);
    }

    #[test]
    fn test_add_device() {
        let mut net_builder = NetBuilder::new();
        let net_id = "test_id";
        let host_dev_name = "dev";
        let guest_mac = "01:23:45:67:89:0b";

        let net = Net::new_with_tap(
            net_id.to_string(),
            host_dev_name.to_string(),
            Some(&MacAddr::parse_str(guest_mac).unwrap()),
            RateLimiter::default(),
            RateLimiter::default(),
        )
        .unwrap();

        net_builder.add_device(Arc::new(Mutex::new(net)));
        assert_eq!(net_builder.net_devices.len(), 1);
        assert_eq!(
            net_builder
                .net_devices
                .pop()
                .unwrap()
                .lock()
                .unwrap()
                .deref()
                .id(),
            net_id
        );
    }
}
