//! Naming conventions. Everything this tool creates is prefixed `vmfleet-` so it
//! can be found and cleaned up without ever touching VMs/runners/units it doesn't
//! own (the safety guard the classifier rightly flagged in the clearbox cleanup).

pub const VM_PREFIX: &str = "vmfleet-";

/// VM instance name: vmfleet-<pool>-<slot>-<ts>-<pid>.
pub fn vm_name(pool: &str, slot: u32, ts: u64, pid: u32) -> String {
    format!("{VM_PREFIX}{pool}-{slot}-{ts}-{pid}")
}

/// Prefix identifying all VMs for a given pool+slot (for orphan sweep).
pub fn slot_vm_prefix(pool: &str, slot: u32) -> String {
    format!("{VM_PREFIX}{pool}-{slot}-")
}

/// Transient worker unit name for a slot.
pub fn worker_unit(slot: u32) -> String {
    format!("vmfleet-worker-{slot}.service")
}

pub const WORKER_UNIT_GLOB: &str = "vmfleet-worker-*.service";
pub const SUPERVISOR_UNIT: &str = "vmfleet-supervisor.service";
pub const GC_SERVICE: &str = "vmfleet-gc.service";
pub const GC_TIMER: &str = "vmfleet-gc.timer";

/// Extract the slot number from a VM name given its owning pool.
/// vmfleet-<pool>-<slot>-... -> slot
pub fn slot_of_vm(pool: &str, vm: &str) -> Option<u32> {
    let head = format!("{VM_PREFIX}{pool}-");
    let rest = vm.strip_prefix(&head)?;
    rest.split('-').next()?.parse::<u32>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_slot() {
        let n = vm_name("small", 103, 1782980000, 4242);
        assert_eq!(n, "vmfleet-small-103-1782980000-4242");
        assert_eq!(slot_of_vm("small", &n), Some(103));
        assert_eq!(slot_of_vm("large", &n), None);
    }
}
