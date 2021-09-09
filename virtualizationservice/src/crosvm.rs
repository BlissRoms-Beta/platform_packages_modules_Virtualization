// Copyright 2021, The Android Open Source Project
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Functions for running instances of `crosvm`.

use crate::aidl::VirtualMachineCallbacks;
use crate::Cid;
use anyhow::{bail, Error};
use command_fds::CommandFdExt;
use log::{debug, error, info};
use shared_child::SharedChild;
use std::fs::{remove_dir_all, File};
use std::num::NonZeroU32;
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use vsock::VsockStream;

const CROSVM_PATH: &str = "/apex/com.android.virt/bin/crosvm";

/// Configuration for a VM to run with crosvm.
#[derive(Debug)]
pub struct CrosvmConfig {
    pub cid: Cid,
    pub bootloader: Option<File>,
    pub kernel: Option<File>,
    pub initrd: Option<File>,
    pub disks: Vec<DiskFile>,
    pub params: Option<String>,
    pub protected: bool,
    pub memory_mib: Option<NonZeroU32>,
    pub log_fd: Option<File>,
    pub indirect_files: Vec<File>,
}

/// A disk image to pass to crosvm for a VM.
#[derive(Debug)]
pub struct DiskFile {
    pub image: File,
    pub writable: bool,
}

/// The lifecycle state which the payload in the VM has reported itself to be in.
///
/// Note that the order of enum variants is significant; only forward transitions are allowed by
/// [`VmInstance::update_payload_state`].
#[derive(Copy, Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum PayloadState {
    Starting,
    Started,
    Ready,
    Finished,
}

/// Information about a particular instance of a VM which is running.
#[derive(Debug)]
pub struct VmInstance {
    /// The crosvm child process.
    child: SharedChild,
    /// The CID assigned to the VM for vsock communication.
    pub cid: Cid,
    /// Whether the VM is a protected VM.
    pub protected: bool,
    /// Directory of temporary files used by the VM while it is running.
    pub temporary_directory: PathBuf,
    /// The UID of the process which requested the VM.
    pub requester_uid: u32,
    /// The SID of the process which requested the VM.
    pub requester_sid: String,
    /// The PID of the process which requested the VM. Note that this process may no longer exist
    /// and the PID may have been reused for a different process, so this should not be trusted.
    pub requester_debug_pid: i32,
    /// Whether the VM is still running.
    running: AtomicBool,
    /// Callbacks to clients of the VM.
    pub callbacks: VirtualMachineCallbacks,
    /// Input/output stream of the payload run in the VM.
    pub stream: Mutex<Option<VsockStream>>,
    /// The latest lifecycle state which the payload reported itself to be in.
    payload_state: Mutex<PayloadState>,
}

impl VmInstance {
    /// Create a new `VmInstance` for the given process.
    fn new(
        child: SharedChild,
        cid: Cid,
        protected: bool,
        temporary_directory: PathBuf,
        requester_uid: u32,
        requester_sid: String,
        requester_debug_pid: i32,
    ) -> VmInstance {
        VmInstance {
            child,
            cid,
            protected,
            temporary_directory,
            requester_uid,
            requester_sid,
            requester_debug_pid,
            running: AtomicBool::new(true),
            callbacks: Default::default(),
            stream: Mutex::new(None),
            payload_state: Mutex::new(PayloadState::Starting),
        }
    }

    /// Start an instance of `crosvm` to manage a new VM. The `crosvm` instance will be killed when
    /// the `VmInstance` is dropped.
    pub fn start(
        config: CrosvmConfig,
        temporary_directory: PathBuf,
        requester_uid: u32,
        requester_sid: String,
        requester_debug_pid: i32,
    ) -> Result<Arc<VmInstance>, Error> {
        let cid = config.cid;
        let protected = config.protected;
        let child = run_vm(config)?;
        let instance = Arc::new(VmInstance::new(
            child,
            cid,
            protected,
            temporary_directory,
            requester_uid,
            requester_sid,
            requester_debug_pid,
        ));

        let instance_clone = instance.clone();
        thread::spawn(move || {
            instance_clone.monitor();
        });

        Ok(instance)
    }

    /// Wait for the crosvm child process to finish, then mark the VM as no longer running and call
    /// any callbacks.
    fn monitor(&self) {
        match self.child.wait() {
            Err(e) => error!("Error waiting for crosvm instance to die: {}", e),
            Ok(status) => info!("crosvm exited with status {}", status),
        }
        self.running.store(false, Ordering::Release);
        self.callbacks.callback_on_died(self.cid);

        // Delete temporary files.
        if let Err(e) = remove_dir_all(&self.temporary_directory) {
            error!("Error removing temporary directory {:?}: {}", self.temporary_directory, e);
        }
    }

    /// Return whether `crosvm` is still running the VM.
    pub fn running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    /// Returns the last reported state of the VM payload.
    pub fn payload_state(&self) -> PayloadState {
        *self.payload_state.lock().unwrap()
    }

    /// Updates the payload state to the given value, if it is a valid state transition.
    pub fn update_payload_state(&self, new_state: PayloadState) -> Result<(), Error> {
        let mut state_locked = self.payload_state.lock().unwrap();
        // Only allow forward transitions, e.g. from starting to started or finished, not back in
        // the other direction.
        if new_state > *state_locked {
            *state_locked = new_state;
            Ok(())
        } else {
            bail!("Invalid payload state transition from {:?} to {:?}", *state_locked, new_state)
        }
    }

    /// Kill the crosvm instance.
    pub fn kill(&self) {
        // TODO: Talk to crosvm to shutdown cleanly.
        if let Err(e) = self.child.kill() {
            error!("Error killing crosvm instance: {}", e);
        }
    }
}

/// Starts an instance of `crosvm` to manage a new VM.
fn run_vm(config: CrosvmConfig) -> Result<SharedChild, Error> {
    validate_config(&config)?;

    let mut command = Command::new(CROSVM_PATH);
    // TODO(qwandor): Remove --disable-sandbox.
    command.arg("run").arg("--disable-sandbox").arg("--cid").arg(config.cid.to_string());

    if config.protected {
        command.arg("--protected-vm");
    }

    if let Some(memory_mib) = config.memory_mib {
        command.arg("--mem").arg(memory_mib.to_string());
    }

    if let Some(log_fd) = config.log_fd {
        command.stdout(log_fd);
    } else {
        // Ignore console output.
        command.arg("--serial=type=sink");
    }

    // Keep track of what file descriptors should be mapped to the crosvm process.
    let mut preserved_fds = config.indirect_files.iter().map(|file| file.as_raw_fd()).collect();

    if let Some(bootloader) = &config.bootloader {
        command.arg("--bios").arg(add_preserved_fd(&mut preserved_fds, bootloader));
    }

    if let Some(initrd) = &config.initrd {
        command.arg("--initrd").arg(add_preserved_fd(&mut preserved_fds, initrd));
    }

    if let Some(params) = &config.params {
        command.arg("--params").arg(params);
    }

    for disk in &config.disks {
        command
            .arg(if disk.writable { "--rwdisk" } else { "--disk" })
            .arg(add_preserved_fd(&mut preserved_fds, &disk.image));
    }

    if let Some(kernel) = &config.kernel {
        command.arg(add_preserved_fd(&mut preserved_fds, kernel));
    }

    debug!("Preserving FDs {:?}", preserved_fds);
    command.preserved_fds(preserved_fds);

    info!("Running {:?}", command);
    let result = SharedChild::spawn(&mut command)?;
    Ok(result)
}

/// Ensure that the configuration has a valid combination of fields set, or return an error if not.
fn validate_config(config: &CrosvmConfig) -> Result<(), Error> {
    if config.bootloader.is_none() && config.kernel.is_none() {
        bail!("VM must have either a bootloader or a kernel image.");
    }
    if config.bootloader.is_some() && (config.kernel.is_some() || config.initrd.is_some()) {
        bail!("Can't have both bootloader and kernel/initrd image.");
    }
    Ok(())
}

/// Adds the file descriptor for `file` to `preserved_fds`, and returns a string of the form
/// "/proc/self/fd/N" where N is the file descriptor.
fn add_preserved_fd(preserved_fds: &mut Vec<RawFd>, file: &File) -> String {
    let fd = file.as_raw_fd();
    preserved_fds.push(fd);
    format!("/proc/self/fd/{}", fd)
}
