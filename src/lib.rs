#![doc = include_str!("../README.md")]

use nix::{
    errno::Errno,
    unistd::{ResGid, ResUid, getresgid, getresuid, setresgid, setresuid},
};
use parking_lot::{Condvar, Mutex};
use std::{cell::RefCell, error, fmt, mem, sync::LazyLock};

/// The Real, Effective, and Saved UID of the application.
pub static USER: LazyLock<ResUid> = LazyLock::new(|| getresuid().expect("Failed to get UID!"));

/// The Real, Effective, and Saved GID of the application.
pub static GROUP: LazyLock<ResGid> = LazyLock::new(|| getresgid().expect("Failed to get GID!"));

/// Whether the system is actually running under setuid. If false, all functions here
/// are no-ops.
pub static SETUID: LazyLock<bool> = LazyLock::new(|| USER.effective != USER.real);

/// The global cooperative state.
///
/// Access to this state is synchronized with `WAKELOCK`. The mutex is held
/// while changing the process mode so that no cooperating caller can observe
/// an intermediate transition.
struct State {
    /// The current mode in the schedule.
    mode: Mode,

    /// How many scopes are using this schedule.
    cooperating: usize,
}

/// The current cooperative state.
///
/// A single process-wide mode may have multiple cooperating scopes, including
/// scopes belonging to different threads.
static STATE: Mutex<State> = Mutex::new(State {
    mode: Mode::Original,
    cooperating: 0,
});

/// A condition variable for threads waiting for the cooperative state to
/// become available for their requested mode.
static WAKELOCK: Condvar = Condvar::new();

thread_local! {
    /// The modes held by `UserLock`s in this thread, from outermost to
    /// innermost.
    ///
    /// This is a stack rather than a counter because a thread may own
    /// multiple nested scopes with different modes:
    ///
    /// When a thread owns the entire cooperative state, changing mode pushes
    /// the new mode onto this stack. Dropping that scope restores the mode
    /// immediately below it.
    static HELD: RefCell<Vec<Mode>> = const { RefCell::new(Vec::new()) };
}

pub struct UserScope;
impl UserScope {
    /// Acquire a new lock for the Operating Mode.
    ///
    /// This object is scoped such that you can guarantee that the `SetUID`
    /// operating mode will be set to `mode` so long as this object is in scope.
    ///
    /// Threads that need the same mode can cooperate, and a single thread can
    /// have multiple different modes nested within one another.
    ///
    /// ## Errors
    ///
    /// If the mode cannot be switched using the underlying
    /// `setresuid`/`setresgid` calls.
    ///
    /// ## Panics
    ///
    /// If the underlying syscall fails
    #[allow(clippy::significant_drop_tightening, clippy::arithmetic_side_effects)]
    pub fn new(mode: Mode) -> Self {
        loop {
            let mut state = STATE.lock();

            let held = HELD.with_borrow(Vec::len);

            // If the state is empty or we own it, change the mode.
            if state.cooperating == 0 || state.cooperating == held {
                let previous = state.mode;
                crate::switch(mode).expect("FATAL: Could not change operating mode");

                HELD.with_borrow_mut(|held| held.push(previous));
                state.mode = mode;
                state.cooperating += 1;

                mem::drop(state);
                WAKELOCK.notify_all();
                return Self {};

            // If we can cooperate, increment the cooperative count.
            } else if state.mode == mode {
                // With cooperation, we push the existing state to prevent
                // a restore in drop.
                HELD.with_borrow_mut(|held| held.push(state.mode));

                state.cooperating += 1;
                return Self {};
            }

            // Wait for the schedule to change.
            WAKELOCK.wait(&mut state);
        }
    }
}

impl Drop for UserScope {
    #[allow(
        clippy::significant_drop_tightening,
        clippy::arithmetic_side_effects,
        clippy::unwrap_used
    )]
    fn drop(&mut self) {
        let mut state = STATE.lock();

        let previous = HELD.with_borrow_mut(|held| held.pop().unwrap());

        // Remove our scope
        state.cooperating -= 1;
        let empty = state.cooperating == 0;

        // If the schedule is changing, revert the mode and alert others
        if empty || state.cooperating == HELD.with_borrow(Vec::len) {
            // Only make a syscall if we need to.
            if state.mode != previous {
                crate::switch(previous).expect("FAILED: Could not restore operating mode");
                state.mode = previous;
            }

            mem::drop(state);

            // If the entire state is changing, notify one thread so it can change + notify_all
            if empty {
                WAKELOCK.notify_one();

            // If our thread has ownership, notify every other thread so they can check for
            // cooperation.
            } else {
                WAKELOCK.notify_all();
            }
        }
    }
}

/// An error when trying to change UID/GID.
#[derive(Debug)]
pub struct Error {
    /// The UID we were trying to change to
    mode: Mode,

    /// The error we got from the syscall.
    errno: Errno,

    /// What syscall we tried to use
    call: &'static str,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{}: Failed to change UID to {}: {}",
            self.call, self.mode, self.errno
        )
    }
}
impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        Some(&self.errno as &dyn error::Error)
    }
}
impl Error {
    #[must_use]
    pub const fn new(mode: Mode, errno: Errno, call: &'static str) -> Self {
        Self { mode, errno, call }
    }
}

/// A `SetUID` mode.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Mode {
    /// Transition to the Real user, setting both Real and Effective
    /// to `USER.real`, while saving Effective to Saved.
    Real,

    /// Transition to the Effective user, setting both Real and Effective
    /// to `USER.effective`, while saving Real to Saved.
    Effective,

    /// The current operating mode. This is functionally a no-op except for
    /// in drop, where it drops whatever the current mode happens to be.
    Existing,

    /// Revert to the program's original operating mode. For set, this
    /// mode is functionally identical to using `revert()`. For drop, it
    /// acts as `user::revert()`.
    Original,
}
impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Real => write!(f, "{}", USER.real),
            Self::Effective => write!(f, "{}", USER.effective),
            Self::Existing => write!(f, "Existing"),
            Self::Original => write!(f, "{}/{}/{}", USER.real, USER.effective, USER.saved),
        }
    }
}

/// Set the current operating mode.
///
/// Note this is *not* thread safe. If you use this while any `UserScope`s are active,
/// you will break it.
///
/// ## Errors
///
/// If the underlying syscall fails.
///
/// ## Panics
///
/// If a `UserScope` is active in any thread
#[allow(clippy::significant_drop_tightening)]
pub fn set(mode: Mode) -> Result<(), Error> {
    if !*SETUID {
        return Ok(());
    }

    let mut lock = STATE.lock();
    assert_eq!(
        lock.cooperating, 0,
        "State cannot be set while a Scope is active"
    );
    lock.mode = mode;
    switch(mode)
}

/// Internal function to switch states.
fn switch(mode: Mode) -> Result<(), Error> {
    if !*SETUID {
        return Ok(());
    }
    match mode {
        Mode::Real => {
            setresuid(USER.real, USER.real, USER.effective)
                .map_err(|e| Error::new(mode, e, "setresuid"))?;
            setresgid(GROUP.real, GROUP.real, GROUP.effective)
                .map_err(|e| Error::new(mode, e, "setresgid"))?;
        }
        Mode::Effective => {
            setresuid(USER.effective, USER.effective, USER.real)
                .map_err(|e| Error::new(mode, e, "setresuid"))?;
            setresgid(GROUP.effective, GROUP.effective, GROUP.real)
                .map_err(|e| Error::new(mode, e, "setresgid"))?;
        }
        Mode::Original => {
            setresuid(USER.real, USER.effective, USER.saved)
                .map_err(|e| Error::new(Mode::Original, e, "setresuid"))?;
            setresgid(GROUP.real, GROUP.effective, GROUP.saved)
                .map_err(|e| Error::new(Mode::Original, e, "setresgid"))?;
        }
        Mode::Existing => {}
    }

    Ok(())
}
/// Destructively change mode, preventing the process from returning.
/// This function will set Real, Effective, and Saved values to the
/// desired Mode. This prevents the process from changing their mode
///
/// ## Errors
/// If the underlying syscall fails
pub fn drop(mode: Mode) -> Result<(), Error> {
    match mode {
        Mode::Real => {
            setresuid(USER.real, USER.real, USER.real)
                .map_err(|e| Error::new(mode, e, "setresuid"))?;
            setresgid(GROUP.real, GROUP.real, GROUP.real)
                .map_err(|e| Error::new(mode, e, "setresgid"))
        }
        Mode::Effective => {
            setresuid(USER.effective, USER.effective, USER.effective)
                .map_err(|e| Error::new(mode, e, "setresuid"))?;
            setresgid(GROUP.effective, GROUP.effective, GROUP.effective)
                .map_err(|e| Error::new(mode, e, "setresgid"))
        }
        Mode::Original => {
            setresuid(USER.real, USER.effective, USER.saved)
                .map_err(|e| Error::new(Mode::Original, e, "setresuid"))?;
            setresgid(GROUP.real, GROUP.effective, GROUP.saved)
                .map_err(|e| Error::new(Mode::Original, e, "setresgid"))
        }
        Mode::Existing => {
            let (user, group) = (
                getresuid().map_err(|e| Error::new(mode, e, "getresuid"))?,
                getresgid().map_err(|e| Error::new(mode, e, "getresgid"))?,
            );
            setresuid(user.real, user.real, user.real)
                .map_err(|e| Error::new(mode, e, "setresuid"))?;
            setresgid(group.real, group.real, group.real)
                .map_err(|e| Error::new(mode, e, "setresgid"))
        }
    }
}

/// Get the current user mode
///
/// Note that this is not thread-safe, and your program can suffer
/// from TOC-TOU problems if you assume this value will remain the same
/// when you actually need to perform a privileged operation in a multi-threaded
/// environment.
///
/// ## Errors
/// If the underlying syscall fails
pub fn current() -> Result<Mode, Error> {
    let uid = getresuid()
        .map_err(|e| Error::new(Mode::Existing, e, "getresuid"))?
        .real;
    if uid == USER.real {
        Ok(Mode::Real)
    } else if uid == USER.effective {
        Ok(Mode::Effective)
    } else {
        Err(Error::new(
            Mode::Existing,
            Errno::EINVAL,
            "current uid unknown",
        ))
    }
}

/// This is a thread-safe wrapper that sets the mode, runs the closure/expression,
/// then returns to the mode before the call.
///
/// You can use this in multi-threaded
/// environments, and it is guaranteed the content of the closure/expression will
/// be run under the requested Mode.
#[macro_export]
macro_rules! run_as {
    ($mode:path, $ret:ty, $body:block) => {{
        {
            let _lock = user::UserScope::new($mode);
            (|| -> $ret { $body })()
        }
    }};

    ($mode:path, $body:block) => {{
        {
            let _lock = user::UserScope::new($mode);
            (|| $body)()
        }
    }};

    ($mode:path, $expr:expr) => {{
        {
            let _lock = user::UserScope::new($mode);
            $expr
        }
    }};
}

#[macro_export]
macro_rules! as_real {
    ($ret:ty, $body:block) => {{ user::run_as!(user::Mode::Real, $ret, $body) }};
    ($body:block) => {{ user::run_as!(user::Mode::Real, $body) }};
    ($expr:expr) => {{ user::run_as!(user::Mode::Real, { $expr }) }};
}

#[macro_export]
macro_rules! as_effective {
    ($ret:ty, $body:block) => {{ user::run_as!(user::Mode::Effective, $ret, $body) }};
    ($body:block) => {{ user::run_as!(user::Mode::Effective, $body) }};
    ($expr:expr) => {{ user::run_as!(user::Mode::Effective, { $expr }) }};
}
