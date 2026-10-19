// GOWA as a child of Coucou. Starts the binary Coucou installed on 127.0.0.1, on the address'
// port, and stops it when Coucou quits — even when Coucou is killed: on Windows the child lives
// in a job object that ends it when the last handle to the job closes, which is when Coucou's
// process goes. Port of WhatsAppGOWAProcess.swift.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::gateway::launch_args;
use crate::log;

/// What GOWA is started with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Launch {
    pub binary: PathBuf,
    pub args: Vec<String>,
    /// "user:password": GOWA's basic auth, passed through the environment, not the command line.
    pub credential: String,
}

impl Launch {
    /// GOWA on this PC, on `port`, without downloading media.
    pub fn gowa(binary: PathBuf, port: u16, credential: String) -> Launch {
        Launch { binary, args: launch_args(port), credential }
    }

    /// What the running GOWA was started with: changed, it is restarted; same, left alone.
    fn signature(&self) -> String {
        format!("{}\u{1f}{}\u{1f}{}", self.binary.display(), self.args.join(" "), self.credential)
    }
}

/// A job object that kills whatever is assigned to it when its last handle closes.
#[cfg(windows)]
struct Job(windows::Win32::Foundation::HANDLE);

// The handle is only ever used from the thread holding the process' mutex, and closed on drop.
#[cfg(windows)]
unsafe impl Send for Job {}

#[cfg(windows)]
impl Job {
    fn holding(child: &Child) -> Option<Job> {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::{CloseHandle, HANDLE};
        use windows::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };
        unsafe {
            let job = CreateJobObjectW(None, windows::core::PCWSTR::null()).ok()?;
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let set = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                std::ptr::addr_of!(info).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            let assigned = set.is_ok() && AssignProcessToJobObject(job, HANDLE(child.as_raw_handle())).is_ok();
            if !assigned {
                let _ = CloseHandle(job);
                return None;
            }
            Some(Job(job))
        }
    }
}

#[cfg(windows)]
impl Drop for Job {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

struct Running {
    child: Child,
    /// Held for as long as the child runs: dropping it ends the child.
    #[cfg(windows)]
    _job: Option<Job>,
}

#[derive(Default)]
struct Inner {
    running: Option<Running>,
    launched_with: Option<String>,
    last_start: Option<Instant>,
}

#[derive(Default)]
pub struct GowaProcess {
    inner: Mutex<Inner>,
}

/// A GOWA that keeps dying is started again once a minute at most.
const RESTART_EVERY: Duration = Duration::from_secs(60);

impl GowaProcess {
    #[cfg(test)]
    pub fn is_running(&self) -> bool {
        let mut inner = self.inner.lock().unwrap();
        Self::alive(&mut inner)
    }

    fn alive(inner: &mut Inner) -> bool {
        match inner.running.as_mut() {
            Some(r) => match r.child.try_wait() {
                Ok(None) => true,
                _ => {
                    inner.running = None;
                    false
                }
            },
            None => false,
        }
    }

    /// Brings GOWA in line with `want`: started when there is something to start, restarted only
    /// when its binary, port or credential changed, stopped when `want` is None. Returns false
    /// when it can't run.
    pub fn sync(&self, want: Option<&Launch>) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let Some(launch) = want else {
            Self::stop(&mut inner);
            return false;
        };
        if !launch.binary.is_file() {
            log::line("WhatsApp: the GOWA binary is missing");
            Self::stop(&mut inner);
            return false;
        }
        let signature = launch.signature();
        if Self::alive(&mut inner) {
            if inner.launched_with.as_deref() == Some(signature.as_str()) {
                return true;
            }
            Self::stop(&mut inner); // waits for it to let go of the port and its database
        } else if inner.launched_with.as_deref() == Some(signature.as_str())
            && inner.last_start.is_some_and(|t| t.elapsed() < RESTART_EVERY)
        {
            return true;
        }

        let dir = launch.binary.parent().map(PathBuf::from).unwrap_or_default();
        let mut command = Command::new(&launch.binary);
        command
            .args(&launch.args)
            .current_dir(&dir)
            // The credential goes through the environment, not the command line (a process list shows that).
            .env("APP_BASIC_AUTH", &launch.credential)
            // GOWA's own web dashboard is not needed (Coucou is the interface): it would fetch
            // itself from GitHub at each start.
            .env("APP_UI_ENABLED", "false")
            // GOWA's own output may name contacts or quote messages: it goes nowhere.
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        match command.spawn() {
            Ok(child) => {
                log::line(format!("WhatsApp: GOWA started (pid {})", child.id()));
                #[cfg(windows)]
                let job = Job::holding(&child);
                #[cfg(windows)]
                if job.is_none() {
                    log::line("WhatsApp: GOWA is not in a job object: it will not end by itself if Coucou is killed");
                }
                inner.running = Some(Running {
                    child,
                    #[cfg(windows)]
                    _job: job,
                });
                inner.last_start = Some(Instant::now());
                inner.launched_with = Some(signature);
                true
            }
            Err(error) => {
                log::line(format!("WhatsApp: GOWA could not start: {error}"));
                false
            }
        }
    }

    /// Stops GOWA if Coucou started it, waiting a second at most for it to go (at quit, or before
    /// starting it again so two never share its database, or before its file is replaced).
    pub fn stop_now(&self) {
        let mut inner = self.inner.lock().unwrap();
        Self::stop(&mut inner);
    }

    fn stop(inner: &mut Inner) {
        inner.launched_with = None;
        let Some(mut running) = inner.running.take() else { return };
        if matches!(running.child.try_wait(), Ok(None)) {
            let _ = running.child.kill();
            let deadline = Instant::now() + Duration::from_secs(1);
            while Instant::now() < deadline && matches!(running.child.try_wait(), Ok(None)) {
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = running.child.wait();
        }
        log::line("WhatsApp: GOWA stopped");
    }
}

impl Drop for GowaProcess {
    fn drop(&mut self) {
        if let Ok(mut inner) = self.inner.lock() {
            Self::stop(&mut inner);
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// A stand-in for GOWA that stays alive for a while: PING.EXE copied under another name, so
    /// the test can't mistake it for anything else, asked to ping the loopback 30 times.
    fn stand_in(dir: &std::path::Path) -> Launch {
        let target = dir.join("gowa-stand-in.exe");
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
        std::fs::copy(PathBuf::from(root).join("System32").join("PING.EXE"), &target).unwrap();
        Launch {
            binary: target,
            args: ["-n", "30", "127.0.0.1"].iter().map(|s| s.to_string()).collect(),
            credential: "coucou:secret".into(),
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("coucou-wa-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn it_starts_once_restarts_on_change_and_stops() {
        let dir = temp_dir("proc");
        let launch = stand_in(&dir);
        let process = GowaProcess::default();

        assert!(process.sync(Some(&launch)));
        assert!(process.is_running());
        let first = process.inner.lock().unwrap().running.as_ref().unwrap().child.id();
        // The same settings: left alone.
        assert!(process.sync(Some(&launch)));
        assert_eq!(process.inner.lock().unwrap().running.as_ref().unwrap().child.id(), first);
        // A changed credential: the old one gives way to a new one.
        let changed = Launch { credential: "coucou:other".into(), ..launch.clone() };
        assert!(process.sync(Some(&changed)));
        assert!(process.is_running());
        assert_ne!(process.inner.lock().unwrap().running.as_ref().unwrap().child.id(), first);
        // A missing binary can't run, and the running one is stopped.
        assert!(!process.sync(Some(&Launch { binary: dir.join("nope.exe"), ..launch.clone() })));
        assert!(!process.is_running());
        // None stops it.
        assert!(process.sync(Some(&launch)));
        assert!(!process.sync(None));
        assert!(!process.is_running());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_gowa_that_dies_is_not_started_again_within_the_minute() {
        let dir = temp_dir("dies");
        let mut launch = stand_in(&dir);
        launch.args = vec!["-not-an-option".into()];
        let process = GowaProcess::default();
        assert!(process.sync(Some(&launch)));
        std::thread::sleep(Duration::from_millis(800));
        assert!(!process.is_running(), "PING.EXE refuses the option and exits");
        let died = process.inner.lock().unwrap().last_start;
        assert!(process.sync(Some(&launch)), "reported as fine: one start a minute at most");
        assert_eq!(process.inner.lock().unwrap().last_start, died);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_job_object_ends_the_child_when_it_closes() {
        let dir = temp_dir("job");
        let launch = stand_in(&dir);
        let mut child = Command::new(&launch.binary).args(&launch.args).stdout(Stdio::null()).spawn().unwrap();
        let job = Job::holding(&child).expect("a job object");
        assert!(matches!(child.try_wait(), Ok(None)));
        drop(job);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && matches!(child.try_wait(), Ok(None)) {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(matches!(child.try_wait(), Ok(Some(_))), "the child went with its job");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
