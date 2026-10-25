//! Keeps the server hot: nothing a call needs should be cold when it arrives.
//!
//! The first PowerShell, the tool lookups, the OS file cache of the tools and the workspace, and the
//! process's own pages are all prepared before the first call and refreshed while the server idles.
//! `QX_WARM=off` turns all of it off.

use std::path::Path;
use std::time::Duration;

const TOOLS: &[&str] = &["git", "node", "cargo", "rg"];
const IDLE_TICK: Duration = Duration::from_secs(10);

pub fn start() {
    if std::env::var("QX_WARM").ok().as_deref() == Some("off") {
        return;
    }
    #[cfg(windows)]
    os::boost_process();
    let _ = std::thread::Builder::new().name("qx-warm".into()).spawn(|| {
        // Preparation never competes with a call for the CPU.
        #[cfg(windows)]
        os::lower_this_thread();
        prime_tools();
        if let Ok(dir) = std::env::current_dir() {
            crate::fsops::prime(&dir);
        }
        loop {
            std::thread::sleep(IDLE_TICK);
            crate::exec::top_up_powershell();
            touch();
        }
    });
}

/// Resolve each tool (filling the resolver cache) and run it once, so its image is in the file cache
/// and the antivirus has already vetted it: the first real `git status` then starts like the tenth.
fn prime_tools() {
    for name in TOOLS {
        let crate::resolve::Target::Direct { exe, prefix } = crate::resolve::resolve(name, None) else {
            continue;
        };
        let mut cmd = std::process::Command::new(&exe);
        cmd.args(&prefix).arg("--version");
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        if let Ok(mut child) = cmd.spawn() {
            let end = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                match child.try_wait() {
                    Ok(None) if std::time::Instant::now() < end => std::thread::sleep(Duration::from_millis(10)),
                    Ok(None) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                    _ => break,
                }
            }
        }
    }
}

/// A little real work on the idle tick keeps the code paths a call uses resident.
fn touch() {
    let _ = crate::builtin::try_exec("echo", &["warm".to_string()], None, &[]);
    let _ = std::fs::metadata(Path::new("."));
}

#[cfg(windows)]
mod os {
    use std::ffi::c_void;

    #[repr(C)]
    struct PowerThrottling {
        version: u32,
        control_mask: u32,
        state_mask: u32,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        fn GetCurrentThread() -> *mut c_void;
        fn SetProcessInformation(h: *mut c_void, class: i32, info: *const c_void, size: u32) -> i32;
        fn SetProcessWorkingSetSize(h: *mut c_void, min: usize, max: usize) -> i32;
        fn SetThreadPriority(h: *mut c_void, p: i32) -> i32;
    }
    #[link(name = "winmm")]
    extern "system" {
        fn timeBeginPeriod(ms: u32) -> u32;
    }

    /// Full speed while idle: Windows otherwise puts a quiet background process in its efficiency
    /// mode (slower clocks, coarse timers) and trims its pages, and the next call pays for the wake-up.
    pub fn boost_process() {
        const PROCESS_POWER_THROTTLING: i32 = 4;
        const EXECUTION_SPEED: u32 = 0x1;
        const IGNORE_TIMER_RESOLUTION: u32 = 0x4;
        let state = PowerThrottling {
            version: 1,
            control_mask: EXECUTION_SPEED | IGNORE_TIMER_RESOLUTION,
            state_mask: 0, // controlled, and switched off
        };
        unsafe {
            let me = GetCurrentProcess();
            SetProcessInformation(
                me,
                PROCESS_POWER_THROTTLING,
                &state as *const _ as *const c_void,
                std::mem::size_of::<PowerThrottling>() as u32,
            );
            // Pages below this stay resident when the system trims an idle process.
            SetProcessWorkingSetSize(me, 24 << 20, 512 << 20);
            timeBeginPeriod(1);
        }
    }

    pub fn lower_this_thread() {
        const THREAD_PRIORITY_BELOW_NORMAL: i32 = -1;
        unsafe {
            SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
        }
    }
}
