//! Thread-scoped idle-sleep inhibition for a connected stream/viewer.
//! No power-plan changes. Windows releases the request if the thread/process exits.
use anyhow::{bail, Result};
use std::marker::PhantomData;
use std::rc::Rc;
use windows::Win32::System::Power::{SetThreadExecutionState, EXECUTION_STATE, ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED};

#[must_use]
pub struct SessionAwake {
    previous: EXECUTION_STATE,
    // The API applies to the calling thread; restore it on that same thread.
    _same_thread: PhantomData<Rc<()>>,
}
impl SessionAwake {
    pub fn acquire() -> Result<Self> {
        let previous = unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED) };
        if previous.0 == 0 { bail!("Windows не разрешила отключить автоматический сон на время трансляции"); }
        crate::net::conn_log("питание: автоматический сон и отключение экрана приостановлены на время сессии");
        Ok(Self { previous, _same_thread: PhantomData })
    }
}
impl Drop for SessionAwake {
    fn drop(&mut self) {
        let restored = unsafe { SetThreadExecutionState(self.previous | ES_CONTINUOUS) };
        crate::net::conn_log(if restored.0 != 0 { "питание: запрос сессии снят, прежние требования потока восстановлены" }
            else { "питание: не удалось снять запрос сессии; Windows снимет его при завершении потока" });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn session_blocks_idle_sleep_and_restores_state_on_error() {
        // Dedicated thread: never modify another running session's execution requirements.
        std::thread::spawn(|| {
            unsafe { SetThreadExecutionState(ES_CONTINUOUS); }
            let fail = || -> Result<()> {
                let _awake = SessionAwake::acquire()?;
                let active = unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED) };
                assert_ne!(active.0 & ES_SYSTEM_REQUIRED.0, 0);
                assert_ne!(active.0 & ES_DISPLAY_REQUIRED.0, 0);
                bail!("simulated session error")
            };
            assert!(fail().is_err());
            let restored = unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
            assert_eq!(restored.0 & (ES_SYSTEM_REQUIRED.0 | ES_DISPLAY_REQUIRED.0), 0);
            // Preserve a pre-existing requirement instead of clearing another guard's state.
            unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED); }
            { let _awake = SessionAwake::acquire().unwrap(); }
            let previous = unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
            assert_ne!(previous.0 & ES_SYSTEM_REQUIRED.0, 0);
            assert_eq!(previous.0 & ES_DISPLAY_REQUIRED.0, 0);
        }).join().unwrap();
    }
}
