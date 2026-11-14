//! Restart the complete guest process after NitroSDK requests a soft reset.
//!
//! Guest threads retain native call stacks. Starting a new worker also resets
//! those stacks, the thread scheduler, graphics and sound together.
use crate::Result;
use std::process::{Command, ExitStatus};

pub const RESET_EXIT_CODE: i32 = 75;
const WORKER: &str = "GEN5_RUNTIME_WORKER";
const SOFT_RESTART: &str = "GEN5_SOFT_RESTART";

pub fn is_worker() -> bool {
    std::env::var_os(WORKER).is_some()
}

pub fn is_soft_restart() -> bool {
    std::env::var(SOFT_RESTART).is_ok_and(|value| value == "1")
}

pub fn is_reset_command(data: u32) -> bool {
    // OSi_SendToPxi(OS_PXI_COMMAND_RESET): command 0x10 in bits 8..14.
    (data >> 8) & 0x7f == 0x10
}

pub fn supervise(args: &[String]) -> Result<()> {
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    supervise_with(|restarting| {
        Command::new(&executable)
            .args(args)
            .env(WORKER, "1")
            .env(SOFT_RESTART, if restarting { "1" } else { "0" })
            .status()
            .map_err(|error| format!("could not start game: {error}"))
    })
}

fn supervise_with(mut start: impl FnMut(bool) -> Result<ExitStatus>) -> Result<()> {
    let mut restarting = false;
    loop {
        let status = start(restarting)?;
        if status.code() == Some(RESET_EXIT_CODE) {
            restarting = true;
            println!("Soft reset: restarting the game from the cartridge.");
            continue;
        }
        return if status.success() {
            Ok(())
        } else {
            Err(format!("game process exited with {status}"))
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_os_reset_command_requests_a_restart() {
        assert!(is_reset_command(0x0004_000c >> 6));
        assert!(!is_reset_command(0x11 << 8));
        assert!(!is_reset_command(0));
    }

    #[test]
    #[cfg(windows)]
    fn restarting_preserves_normal_exit_and_propagates_launch_errors() {
        use std::os::windows::process::ExitStatusExt;
        let mut attempts = 0;
        supervise_with(|restarting| {
            assert_eq!(restarting, attempts != 0);
            attempts += 1;
            Ok(ExitStatus::from_raw(if attempts < 3 {
                RESET_EXIT_CODE as u32
            } else {
                0
            }))
        })
        .unwrap();
        assert_eq!(attempts, 3);
        assert!(supervise_with(|_| Ok(ExitStatus::from_raw(1))).is_err());
        assert_eq!(
            supervise_with(|_| Err("launch failed".into())),
            Err("launch failed".into())
        );
    }
}
