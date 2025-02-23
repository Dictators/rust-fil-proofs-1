use std::sync::{Mutex, MutexGuard};

use anyhow::{format_err, Result};
use hwloc::{Bitmap, ObjectType, Topology, TopologyObject, CPUBIND_THREAD};
use lazy_static::lazy_static;
use log::{debug, warn};
use storage_proofs_core::settings::SETTINGS;

type CoreGroup = Vec<CoreIndex>;
lazy_static! {
    pub static ref TOPOLOGY: Mutex<Topology> = Mutex::new(Topology::new());
    // pub static ref CORE_GROUPS: Option<Vec<Mutex<CoreGroup>>> = {
    pub static ref CORE_GROUPS: Vec<Mutex<CoreGroup>> = {
        let num_producers = &SETTINGS.multicore_sdr_producers;
        let cores_per_unit = num_producers + 1;

        core_groups(cores_per_unit)
    };
}

#[derive(Clone, Copy, Debug, PartialEq)]
/// `CoreIndex` is a simple wrapper type for indexes into the set of vixible cores. A `CoreIndex` should only ever be
/// created with a value known to be less than the number of visible cores.
pub struct CoreIndex(usize);

pub fn checkout_core_group() -> Option<MutexGuard<'static, CoreGroup>> {
    // match &*CORE_GROUPS {
    //     Some(groups) => {
    //         for (i, group) in groups.iter().enumerate() {
    //             match group.try_lock() {
    //                 Ok(guard) => {
    //                     debug!("checked out core group {}", i);
    //                     return Some(guard);
    //                 }
    //                 Err(_) => debug!("core group {} locked, could not checkout", i),
    //             }
    //         }
    //         None
    //     }
    //     None => None,
    // }
    for (i, group) in CORE_GROUPS.iter().enumerate() {
        match group.try_lock() {
            Ok(guard) => {
                debug!("checked out core group {}", i);
                return Some(guard);
            }
            Err(_) => debug!("core group {} locked, could not checkout", i),
        }
    }
    None
}

#[cfg(not(target_os = "windows"))]
pub type ThreadId = libc::pthread_t;

#[cfg(target_os = "windows")]
pub type ThreadId = winapi::winnt::HANDLE;

/// Helper method to get the thread id through libc, with current rust stable (1.5.0) its not
/// possible otherwise I think.
#[cfg(not(target_os = "windows"))]
fn get_thread_id() -> ThreadId {
    unsafe { libc::pthread_self() }
}

#[cfg(target_os = "windows")]
fn get_thread_id() -> ThreadId {
    unsafe { kernel32::GetCurrentThread() }
}

pub struct Cleanup {
    tid: ThreadId,
    prior_state: Option<Bitmap>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Some(prior) = self.prior_state.take() {
            let child_topo = &TOPOLOGY;
            let mut locked_topo = child_topo.lock().expect("poisded lock");
            // Modified by long 20210708
            let _ = locked_topo.set_cpubind_for_thread(self.tid, prior.clone(), CPUBIND_THREAD);
            let _ = locked_topo.set_membind(prior, hwloc::MEMBIND_DEFAULT, hwloc::MEMBIND_THREAD);
        }
    }
}

pub fn bind_core(core_index: CoreIndex) -> Result<Cleanup> {
    let child_topo = &TOPOLOGY;
    let tid = get_thread_id();
    let mut locked_topo = child_topo.lock().expect("poisoned lock");
    let core = get_core_by_index(&locked_topo, core_index)
        .map_err(|err| format_err!("failed to get core at index {}: {:?}", core_index.0, err))?;

    let cpuset = core
        .allowed_cpuset()
        .ok_or_else(|| format_err!("no allowed cpuset for core at index {}", core_index.0,))?;
    debug!("allowed cpuset: {:?}", cpuset);
    let mut bind_to = cpuset;

    // Get only one logical processor (in case the core is SMT/hyper-threaded).
    bind_to.singlify();

    // Thread binding before explicit set.
    let before = locked_topo.get_cpubind_for_thread(tid, CPUBIND_THREAD);

    debug!("binding to {:?}", bind_to);
    // Set the binding.
    let result = locked_topo
        // Modified by long 20210708
        .set_cpubind_for_thread(tid, bind_to.clone(), CPUBIND_THREAD)
        .map_err(|err| format_err!("failed to bind CPU: {:?}", err));

    if result.is_err() {
        warn!("error in bind_core, {:?}", result);
    }

    // Added by long 20210708
    let _ = locked_topo.set_membind(bind_to, hwloc::MEMBIND_BIND, hwloc::MEMBIND_THREAD);

    Ok(Cleanup {
        tid,
        prior_state: before,
    })
}

fn get_core_by_index(topo: &Topology, index: CoreIndex) -> Result<&TopologyObject> {
    let idx = index.0;

    match topo.objects_with_type(&ObjectType::Core) {
        Ok(all_cores) if idx < all_cores.len() => Ok(all_cores[idx]),
        Ok(all_cores) => Err(format_err!(
            "idx ({}) out of range for {} cores",
            idx,
            all_cores.len()
        )),
        _e => Err(format_err!("failed to get core by index {}", idx,)),
    }
}

// fn core_groups(cores_per_unit: usize) -> Option<Vec<Mutex<Vec<CoreIndex>>>> {
fn core_groups(cores_per_unit: usize) -> Vec<Mutex<Vec<CoreIndex>>> {
    let topo = TOPOLOGY.lock().expect("poisoned lock");
    let all_cores = topo
        .objects_with_type(&ObjectType::Core)
        .expect("objects_with_type failed");
    let core_count = all_cores.len();
    let group_count = core_count / cores_per_unit;
    let group_size = cores_per_unit;

    let core_groups = (0..group_count)
        .rev()
        .map(|i| {
            (0..group_size)
                .map(|j| {
                    let core_index = i * group_size + j;
                    assert!(core_index < core_count);
                    CoreIndex(core_index)
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();

    // Some(
    core_groups
        .iter()
        .map(|group| Mutex::new(group.clone()))
        .collect::<Vec<_>>()
    // )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cores() {
        core_groups(2);
    }

    #[test]
    #[cfg(feature = "isolated-testing")]
    // This test should not be run while other tests are running, as
    // the cores we're working with may otherwise be busy and cause a
    // failure.
    fn test_checkout_cores() {
        let checkout1 = checkout_core_group();
        dbg!(&checkout1);
        let checkout2 = checkout_core_group();
        dbg!(&checkout2);

        // This test might fail if run on a machine with fewer than four cores.
        match (checkout1, checkout2) {
            (Some(c1), Some(c2)) => assert!(*c1 != *c2),
            _ => panic!("failed to get two checkouts"),
        }
    }
}
