//! Reproduces Plex's confinement and asks CUDA to initialise inside it.
//!
//! Written because the appliance had run out of cheap answers. Under Landlock, Plex could
//! not create a CUDA context — "opening hw device failed ... Generic error in an external
//! library" — while the same transcoder, as the same uid, in the same cgroup, without
//! Landlock, initialised the device fine. Every path `libcuda` names was granted and the
//! confinement log confirmed every rule applied.
//!
//! At that point the next honest step is not another hypothesis costing a two-hour build
//! and a reboot. It is a way to bisect the policy in seconds, on a machine that has an
//! NVIDIA card and a compiler — which the build host does.
//!
//! ```text
//! cargo run --example cuda-under-landlock              # the full policy
//! cargo run --example cuda-under-landlock -- --none    # no Landlock, the control
//! cargo run --example cuda-under-landlock -- --drop /sys
//! ```
//!
//! `--drop` removes one grant, so a policy that works can be walked backwards until it
//! stops. The point is to find the *one* path whose absence breaks it, rather than to
//! confirm that the whole set works — which is already known and is not the question.

use std::ffi::{CString, c_int, c_void};
use std::path::PathBuf;

/// The grants Plex gets, in the order `plexos_plex::run::grants` produces them.
///
/// Duplicated deliberately rather than imported: this crate is below `plexos-plex` and
/// must not depend upward. If the two drift, this example stops reproducing the thing it
/// exists to reproduce — so it prints the list it used, and the operator can compare.
fn policy() -> Vec<(&'static str, u64)> {
    use plexos_sys::landlock::access;
    vec![
        ("/usr", access::READ_EXECUTE),
        ("/etc", access::READ_ONLY),
        (
            "/proc",
            access::READ_ONLY | access::WRITE_FILE | access::TRUNCATE,
        ),
        (
            "/dev",
            access::READ_FILE | access::WRITE_FILE | access::READ_DIR,
        ),
        ("/run", access::READ_ONLY),
        ("/sys", access::READ_ONLY),
        (
            "/dev/dri",
            access::READ_FILE | access::WRITE_FILE | access::READ_DIR | access::IOCTL_DEV,
        ),
        (
            "/dev/nvidiactl",
            access::READ_FILE | access::WRITE_FILE | access::IOCTL_DEV,
        ),
        (
            "/dev/nvidia0",
            access::READ_FILE | access::WRITE_FILE | access::IOCTL_DEV,
        ),
        (
            "/dev/nvidia-uvm",
            access::READ_FILE | access::WRITE_FILE | access::IOCTL_DEV,
        ),
        (
            "/dev/nvidia-uvm-tools",
            access::READ_FILE | access::WRITE_FILE | access::IOCTL_DEV,
        ),
        (
            "/dev/nvidia-caps",
            access::READ_FILE | access::READ_DIR | access::IOCTL_DEV,
        ),
    ]
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let confine = !args.iter().any(|a| a == "--none");
    let added: Vec<&str> = args
        .windows(2)
        .filter(|pair| pair[0] == "--add")
        .map(|pair| pair[1].as_str())
        .collect();
    let dropped: Vec<&str> = args
        .windows(2)
        .filter(|pair| pair[0] == "--drop")
        .map(|pair| pair[1].as_str())
        .collect();

    if confine {
        let mut ruleset =
            match plexos_sys::landlock::Ruleset::new(plexos_sys::landlock::access::ALL) {
                Ok(ruleset) => ruleset,
                Err(error) => {
                    eprintln!("no Landlock on this kernel: {error}");
                    return;
                }
            };
        for (path, rights) in policy() {
            if dropped.contains(&path) {
                println!("dropped  {path}");
                continue;
            }
            match ruleset.allow(&PathBuf::from(path), rights) {
                Ok(()) => println!("granted  {path}"),
                Err(error) => println!("skipped  {path}: {error}"),
            }
        }
        let extra: u64 = args
            .windows(2)
            .find(|pair| pair[0] == "--rights")
            .and_then(|pair| u64::from_str_radix(pair[1].trim_start_matches("0x"), 16).ok())
            .unwrap_or(plexos_sys::landlock::access::ALL);
        for path in &added {
            match ruleset.allow(&PathBuf::from(path), extra) {
                Ok(()) => println!("added    {path}"),
                Err(error) => println!("skipped  {path}: {error}"),
            }
        }
        match ruleset.enforce() {
            Ok(()) => println!("-- policy applied"),
            Err(error) => {
                eprintln!("could not apply the policy: {error}");
                return;
            }
        }
    } else {
        println!("-- no Landlock (control run)");
    }

    // dlopen rather than linking: this example must build on a machine with no NVIDIA
    // libraries at all, which every other machine in this project is.
    let name = CString::new("libcuda.so.1").expect("a literal with no NUL");
    // SAFETY: dlopen takes a NUL-terminated string that outlives the call and returns a
    // handle or null. Nothing here dereferences the handle except through dlsym below.
    let handle = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW) };
    if handle.is_null() {
        // SAFETY: dlerror returns a pointer to a static message or null, valid until the
        // next dl call in this thread.
        let reason = unsafe { libc::dlerror() };
        let reason = if reason.is_null() {
            "unknown".to_owned()
        } else {
            // SAFETY: dlerror gave a NUL-terminated string.
            unsafe { std::ffi::CStr::from_ptr(reason) }
                .to_string_lossy()
                .into_owned()
        };
        println!("RESULT: libcuda.so.1 could not be loaded: {reason}");
        return;
    }

    let symbol = CString::new("cuInit").expect("a literal with no NUL");
    // SAFETY: the handle came from dlopen and the name is NUL-terminated.
    let address = unsafe { libc::dlsym(handle, symbol.as_ptr()) };
    if address.is_null() {
        println!("RESULT: libcuda.so.1 has no cuInit");
        return;
    }

    // SAFETY: cuInit is `CUresult cuInit(unsigned int)` in every release of the driver
    // API since it existed. The transmute matches that signature exactly.
    let cu_init: extern "C" fn(c_int) -> c_int = unsafe { std::mem::transmute(address) };
    let result = cu_init(0);

    // 0 is CUDA_SUCCESS. 100 is CUDA_ERROR_NO_DEVICE, 999 is CUDA_ERROR_UNKNOWN, which is
    // what a denied ioctl surfaces as and what ffmpeg reports as "Generic error in an
    // external library".
    println!(
        "RESULT: cuInit returned {result}{}",
        match result {
            0 => " (success)",
            100 => " (no device)",
            999 => " (unknown -- this is what the appliance shows)",
            _ => "",
        }
    );

    let _: *mut c_void = handle;
}
