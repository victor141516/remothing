//! The GPU's name for the admin status. macOS publishes no utilization
//! figure an unprivileged process can read cheaply, so none is given.

use objc2_metal::{MTLCreateSystemDefaultDevice, MTLDevice};
use remothing_core::platform::{GpuInfo, SystemMonitor};

pub(crate) struct MacMonitor {
    gpu: Option<String>,
}

impl MacMonitor {
    pub fn new() -> Self {
        Self { gpu: MTLCreateSystemDefaultDevice().map(|d| d.name().to_string()) }
    }
}

impl SystemMonitor for MacMonitor {
    fn gpus(&self) -> Vec<GpuInfo> {
        self.gpu.iter().map(|name| GpuInfo { name: name.clone(), vendor: "Apple".into(), utilization: None }).collect()
    }
}
