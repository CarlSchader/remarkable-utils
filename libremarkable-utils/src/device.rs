//! reMarkable device connection description.

use serde::{Deserialize, Serialize};

/// Default hostname of a reMarkable tablet when connected over USB.
pub const DEFAULT_USB_HOST: &str = "10.11.99.1";

/// Default SSH user on the tablet.
pub const DEFAULT_SSH_USER: &str = "root";

/// Path on the tablet where xochitl stores documents.
pub const XOCHITL_DATA_DIR: &str = "/home/root/.local/share/remarkable/xochitl";

/// A reachable reMarkable device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    /// Hostname or IP address of the tablet.
    pub host: String,
    /// SSH user (normally `root`).
    pub user: String,
}

impl Default for Device {
    fn default() -> Self {
        Self {
            host: DEFAULT_USB_HOST.to_string(),
            user: DEFAULT_SSH_USER.to_string(),
        }
    }
}

impl Device {
    /// `user@host` string suitable for ssh/scp/rsync invocations.
    pub fn ssh_target(&self) -> String {
        format!("{}@{}", self.user, self.host)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_ssh_target() {
        assert_eq!(Device::default().ssh_target(), "root@10.11.99.1");
    }
}
