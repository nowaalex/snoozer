#![deny(unsafe_code)]

pub(crate) mod amd;
pub(crate) mod intel;

use crate::Capabilities;

pub(crate) fn detect_capabilities() -> Capabilities {
    let amd = amd::detect_capabilities();
    let intel = intel::detect_capabilities();
    Capabilities {
        supported_target: true,
        amd_vendor: amd.amd_vendor,
        intel_vendor: intel.intel_vendor,
        monitorx_mwaitx: amd.monitorx_mwaitx,
        waitpkg: intel.waitpkg,
        invariant_tsc: if amd.amd_vendor {
            amd.invariant_tsc
        } else {
            intel.invariant_tsc
        },
        rdtscp: if amd.amd_vendor {
            amd.rdtscp
        } else {
            intel.rdtscp
        },
    }
}
