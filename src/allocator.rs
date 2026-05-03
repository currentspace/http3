//! Native allocator selection for the Node addon.
//!
//! Sustained QUIC/H3 workloads churn many medium-sized native buffers. Jemalloc
//! gives us background purging and bounded arena count without relying on the
//! host application's environment variables.

use std::time::Duration;

#[cfg(all(
    feature = "native-jemalloc",
    not(target_env = "msvc"),
    target_os = "linux"
))]
const JEMALLOC_CONF: &[u8] =
    b"background_thread:true,dirty_decay_ms:5000,muzzy_decay_ms:5000,narenas:4,abort_conf:true\0";

#[cfg(all(
    feature = "native-jemalloc",
    not(target_env = "msvc"),
    target_os = "macos"
))]
const JEMALLOC_CONF: &[u8] = b"dirty_decay_ms:5000,muzzy_decay_ms:5000,narenas:4,abort_conf:true\0";

#[cfg(all(
    feature = "native-jemalloc",
    not(target_env = "msvc"),
    any(target_os = "linux", target_os = "macos")
))]
union JemallocConfPtr {
    bytes: &'static u8,
    c_char: &'static libc::c_char,
}

#[cfg(all(
    feature = "native-jemalloc",
    not(target_env = "msvc"),
    any(target_os = "linux", target_os = "macos")
))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(all(
    feature = "native-jemalloc",
    not(target_env = "msvc"),
    any(target_os = "linux", target_os = "macos")
))]
#[allow(unsafe_code)]
#[unsafe(export_name = "_rjem_malloc_conf")]
pub static JEMALLOC_MALLOC_CONF: Option<&'static libc::c_char> = Some(unsafe {
    // SAFETY: JEMALLOC_CONF is a static NUL-terminated byte string. jemalloc
    // reads `_rjem_malloc_conf` as `const char *` during allocator
    // initialization, before Rust code can set runtime options.
    JemallocConfPtr {
        bytes: &JEMALLOC_CONF[0],
    }
    .c_char
});

/// Give jemalloc a chance to return unused native pages to the OS.
///
/// Linux builds use jemalloc background purging. macOS accepts the decay
/// settings but does not support jemalloc background purge threads in this
/// crate build, so the native worker periodically issues explicit arena purge
/// commands off the JS event loop.
pub(crate) fn maintenance_tick() {
    maintenance_tick_impl();
}

/// How often native worker loops should run allocator maintenance.
///
/// Linux jemalloc builds use `background_thread:true` in static config, so this
/// interval is only a low-cost safety net. macOS accepts decay settings but not
/// background purge threads in this build; keep explicit arena purges infrequent
/// so they nudge RSS down over time without creating hot-path mmap/munmap churn.
pub(crate) fn maintenance_interval() -> Duration {
    maintenance_interval_impl()
}

#[cfg(all(
    feature = "native-jemalloc",
    not(target_env = "msvc"),
    target_os = "macos"
))]
fn maintenance_interval_impl() -> Duration {
    Duration::from_secs(10)
}

#[cfg(not(all(
    feature = "native-jemalloc",
    not(target_env = "msvc"),
    target_os = "macos"
)))]
fn maintenance_interval_impl() -> Duration {
    Duration::from_secs(5)
}

#[cfg(all(
    feature = "native-jemalloc",
    not(target_env = "msvc"),
    target_os = "macos"
))]
fn maintenance_tick_impl() {
    let _ = tikv_jemalloc_ctl::epoch::advance();

    let Ok(narenas) = tikv_jemalloc_ctl::arenas::narenas::read() else {
        return;
    };

    for arena in 0..narenas {
        purge_arena(arena);
    }
}

#[cfg(all(
    feature = "native-jemalloc",
    not(target_env = "msvc"),
    target_os = "macos"
))]
#[allow(unsafe_code)]
fn purge_arena(arena: u32) {
    let name = format!("arena.{arena}.purge\0");
    // SAFETY: `name` is NUL-terminated and lives for the duration of the
    // mallctl call. The `arena.<i>.purge` command has no payload; the ctl crate
    // models command-style mallctls through a zero-sized write.
    let _ = unsafe { tikv_jemalloc_ctl::raw::write(name.as_bytes(), ()) };
}

#[cfg(not(all(
    feature = "native-jemalloc",
    not(target_env = "msvc"),
    target_os = "macos"
)))]
fn maintenance_tick_impl() {}

#[cfg(test)]
mod tests {
    #[cfg(all(
        feature = "native-jemalloc",
        not(target_env = "msvc"),
        any(target_os = "linux", target_os = "macos")
    ))]
    #[test]
    #[allow(unsafe_code)]
    fn jemalloc_config_is_static_and_nul_terminated() {
        use std::ffi::CStr;

        let conf = super::JEMALLOC_MALLOC_CONF.expect("jemalloc config");
        // SAFETY: the config test reads the same static NUL-terminated string
        // exported to jemalloc above.
        let conf = unsafe { CStr::from_ptr(conf) }
            .to_str()
            .expect("utf8 config");

        if cfg!(target_os = "linux") {
            assert!(conf.contains("background_thread:true"));
        } else {
            assert!(!conf.contains("background_thread"));
        }
        assert!(conf.contains("dirty_decay_ms:5000"));
        assert!(conf.contains("muzzy_decay_ms:5000"));
        assert!(conf.contains("narenas:4"));
        assert!(conf.contains("abort_conf:true"));
    }

    #[test]
    fn allocator_maintenance_tick_is_safe_to_call() {
        super::maintenance_tick();
    }

    #[test]
    fn allocator_maintenance_interval_is_nonzero() {
        assert!(super::maintenance_interval() > std::time::Duration::ZERO);
    }
}
