//! The `GameProcess` port: what runs, starting the game, bringing it forward.
//! Ported from Waypoint's `Spawn.swift`, `RunningProcesses` and `BattleNet`
//! (0BSD).

use std::ffi::{CString, c_char, c_int, c_void};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr;

use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication};
use objc2_foundation::{NSBundle, NSString};
use stoneform_app::ports::{GameProcess, Running, SystemError};

/// Finds, starts and shows processes with libproc, `posix_spawn` and AppKit.
#[derive(Debug, Clone, Copy, Default)]
pub struct MacGameProcess;

#[derive(Debug, thiserror::Error)]
enum ProcessError {
    #[error("{} is not an app with an executable", .0.display())]
    NoExecutable(PathBuf),
    #[error("{0} contains a NUL byte")]
    Nul(String),
    #[error("could not start the game: {0}")]
    Spawn(io::Error),
}

impl GameProcess for MacGameProcess {
    fn running(&self, install_root: &Path) -> Running {
        let mut prefix = kernel_path(install_root).unwrap_or_else(|| install_root.to_path_buf()).into_os_string();
        prefix.push("/");
        let prefix = prefix.as_bytes();
        let mut running = Running::default();
        for (pid, path) in executables() {
            let bytes = path.as_os_str().as_bytes();
            if running.game.is_none() && bytes.starts_with(prefix) {
                running.game = Some(pid);
            }
            running.battle_net |= is_battle_net(bytes);
        }
        running
    }

    fn spawn(&self, app: &Path, arguments: &[String], working_directory: &Path) -> Result<u32, SystemError> {
        let executable = NSBundle::bundleWithPath(&NSString::from_str(&app.to_string_lossy()))
            .and_then(|bundle| bundle.executablePath())
            .map(|path| PathBuf::from(path.to_string()))
            .ok_or_else(|| SystemError::new(ProcessError::NoExecutable(app.to_owned())))?;
        spawn_detached(&executable, arguments, working_directory).map_err(SystemError::new)
    }

    fn activate(&self, pid: u32) -> bool {
        let Ok(pid) = libc::pid_t::try_from(pid) else { return false };
        NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
            .is_some_and(|app| app.activateWithOptions(NSApplicationActivationOptions::empty()))
    }
}

/// The Battle.net app (or a helper inside it), or the Agent that patches games
/// in the background from `/Users/Shared/Battle.net/Agent/Agent.<build>/`.
fn is_battle_net(path: &[u8]) -> bool {
    let contains = |needle: &[u8]| path.windows(needle.len()).any(|window| window == needle);
    contains(b"/Battle.net.app/Contents/")
        || (contains(b"/Battle.net/Agent/") && path.ends_with(b"/Contents/MacOS/Agent"))
}

/// Every process's executable path that we may see.
fn executables() -> Vec<(u32, PathBuf)> {
    // SAFETY: a null buffer asks for the count; then the buffer is as large as said.
    let count = unsafe { libc::proc_listallpids(ptr::null_mut(), 0) };
    let capacity = usize::try_from(count).unwrap_or(0) + 64;
    let mut pids = vec![0 as libc::pid_t; capacity];
    let bytes = c_int::try_from(capacity * size_of::<libc::pid_t>()).unwrap_or(c_int::MAX);
    let listed = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    pids.truncate(usize::try_from(listed).unwrap_or(0));
    let mut buffer = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    pids.into_iter()
        .filter(|&pid| pid > 0)
        .filter_map(|pid| {
            // SAFETY: the buffer is PROC_PIDPATHINFO_MAXSIZE bytes, as passed.
            let length = unsafe { libc::proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
            let length = usize::try_from(length).ok().filter(|&length| length > 0)?;
            let path = PathBuf::from(std::ffi::OsStr::from_bytes(&buffer[..length]));
            Some((u32::try_from(pid).ok()?, path))
        })
        .collect()
}

/// The path the kernel has for `path`, spelled as `proc_pidpath` spells it:
/// links resolved (`/private/tmp`, not `/tmp`) and in the volume's letter case.
fn kernel_path(path: &Path) -> Option<PathBuf> {
    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: O_EVTONLY only asks about the file; the descriptor is closed below.
    let descriptor = unsafe { libc::open(c_path.as_ptr(), libc::O_EVTONLY | libc::O_CLOEXEC) };
    if descriptor < 0 {
        return None;
    }
    let mut buffer = vec![0u8; libc::PATH_MAX as usize];
    let result = unsafe { libc::fcntl(descriptor, libc::F_GETPATH, buffer.as_mut_ptr()) };
    unsafe { libc::close(descriptor) };
    if result != 0 {
        return None;
    }
    let length = buffer.iter().position(|&byte| byte == 0)?;
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(&buffer[..length])))
}

type Disclaim = unsafe extern "C" fn(*mut libc::posix_spawnattr_t, c_int) -> c_int;

/// `responsibility_spawnattrs_setdisclaim`: a private but long-stable libsystem
/// call, looked up at run time so a macOS without it falls back to a plain spawn.
fn disclaim_function() -> Option<Disclaim> {
    // SAFETY: the symbol, when present, has exactly this signature.
    unsafe {
        let symbol = libc::dlsym(libc::RTLD_DEFAULT, c"responsibility_spawnattrs_setdisclaim".as_ptr());
        (!symbol.is_null()).then(|| std::mem::transmute::<*mut c_void, Disclaim>(symbol))
    }
}

/// Starts `executable` as its own, independent process: its own process
/// group (Ctrl-C in our terminal doesn't reach it), its own responsible
/// process (so WoW's microphone prompt is the game's, not ours), no
/// inherited descriptors, and `working_directory` as its current folder.
fn spawn_detached(executable: &Path, arguments: &[String], working_directory: &Path) -> Result<u32, ProcessError> {
    let c_string =
        |text: &[u8]| CString::new(text).map_err(|_| ProcessError::Nul(String::from_utf8_lossy(text).into_owned()));
    let program = c_string(executable.as_os_str().as_bytes())?;
    let directory = c_string(working_directory.as_os_str().as_bytes())?;
    let mut argv_owned = vec![program.clone()];
    for argument in arguments {
        argv_owned.push(c_string(argument.as_bytes())?);
    }
    let mut argv: Vec<*mut c_char> = argv_owned.iter().map(|argument| argument.as_ptr().cast_mut()).collect();
    argv.push(ptr::null_mut());

    let mut pid: libc::pid_t = 0;
    // SAFETY: attributes and actions are initialised before use and destroyed
    // after; every string outlives the call; environ is the process's own.
    let result = unsafe {
        let mut attributes: libc::posix_spawnattr_t = ptr::null_mut();
        libc::posix_spawnattr_init(&mut attributes);
        let flags = libc::POSIX_SPAWN_SETPGROUP | libc::POSIX_SPAWN_CLOEXEC_DEFAULT;
        libc::posix_spawnattr_setflags(&mut attributes, flags as libc::c_short);
        libc::posix_spawnattr_setpgroup(&mut attributes, 0);
        if let Some(disclaim) = disclaim_function() {
            disclaim(&mut attributes, 1);
        }
        let mut actions: libc::posix_spawn_file_actions_t = ptr::null_mut();
        libc::posix_spawn_file_actions_init(&mut actions);
        libc::posix_spawn_file_actions_addchdir_np(&mut actions, directory.as_ptr());
        for fd in 0..3 {
            let mode = if fd == 0 { libc::O_RDONLY } else { libc::O_WRONLY };
            libc::posix_spawn_file_actions_addopen(&mut actions, fd, c"/dev/null".as_ptr(), mode, 0);
        }
        let environment = *libc::_NSGetEnviron();
        let result = libc::posix_spawn(&mut pid, program.as_ptr(), &actions, &attributes, argv.as_ptr(), environment);
        libc::posix_spawn_file_actions_destroy(&mut actions);
        libc::posix_spawnattr_destroy(&mut attributes);
        result
    };
    if result != 0 {
        return Err(ProcessError::Spawn(io::Error::from_raw_os_error(result)));
    }
    // Reap the child when it exits, so it never lingers as a zombie while
    // StoneForm keeps running.
    std::thread::spawn(move || {
        let mut status = 0;
        unsafe { libc::waitpid(pid, &mut status, 0) };
    });
    u32::try_from(pid).map_err(|_| ProcessError::Spawn(io::Error::other("negative process id")))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn recognises_battle_net_and_its_agent() {
        assert!(is_battle_net(b"/Applications/Battle.net.app/Contents/MacOS/Battle.net"));
        assert!(is_battle_net(b"/Applications/Battle.net.app/Contents/Frameworks/Helper.app/Contents/MacOS/Helper"));
        assert!(is_battle_net(b"/Users/Shared/Battle.net/Agent/Agent.9172/Agent.app/Contents/MacOS/Agent"));
        assert!(!is_battle_net(
            b"/Applications/World of Warcraft/_forever_/World of Warcraft.app/Contents/MacOS/World of Warcraft"
        ));
        assert!(!is_battle_net(b"/Users/Shared/Battle.net/Agent/Agent.9172/Agent.app/Contents/MacOS/Uninstaller"));
    }

    #[test]
    fn spells_paths_as_the_kernel_does() {
        let dir = tempfile::tempdir().unwrap();
        let spelled = kernel_path(dir.path()).unwrap();
        // macOS temp folders live under /private; the kernel path says so.
        assert!(spelled.starts_with("/private") || spelled == dir.path(), "{}", spelled.display());
        assert_eq!(kernel_path(Path::new("/no/such/folder")), None);
    }

    /// Starts a copy of `sleep` from inside a folder and finds it there.
    #[test]
    fn finds_a_process_started_from_the_install_folder() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("sleeper");
        std::fs::copy("/bin/sleep", &executable).unwrap();
        assert_eq!(MacGameProcess.running(root.path()).game, None);

        let pid = spawn_detached(&executable, &["5".to_owned()], root.path()).unwrap();

        let deadline = Instant::now() + Duration::from_secs(3);
        let mut found = MacGameProcess.running(root.path()).game;
        while found.is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
            found = MacGameProcess.running(root.path()).game;
        }
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        assert_eq!(found, Some(pid));
    }

    #[test]
    fn reports_an_app_without_an_executable() {
        let dir = tempfile::tempdir().unwrap();
        let error = MacGameProcess.spawn(&dir.path().join("Missing.app"), &[], dir.path()).unwrap_err();
        assert!(error.to_string().contains("not an app"), "{error}");
    }
}
