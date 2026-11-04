//! Block across fork-like clone on the calling thread only. Use the kernel ABI
//! so libc's reserved NPTL signals (32/33) cannot run inherited handlers either.
use super::*;

pub(super) struct Blocked {
    previous: u64,
}

impl Blocked {
    pub(super) fn new() -> Result<Self> {
        let all = u64::MAX;
        let mut previous = 0;
        ensure!(
            unsafe {
                libc::syscall(
                    libc::SYS_rt_sigprocmask,
                    libc::SIG_SETMASK,
                    &all,
                    &mut previous,
                    8,
                )
            } == 0,
            "cannot block signals around clone3"
        );
        Ok(Self { previous })
    }
}

impl Drop for Blocked {
    fn drop(&mut self) {
        // Parent only: the clone child must take its nonreturning syscall path,
        // reset dispositions, then unmask; never restore the inherited mask.
        // This valid SIG_SETMASK syscall cannot fail on our supported ABI after
        // new() succeeded. No fallible parent operation intervenes after clone.
        unsafe {
            libc::syscall(
                libc::SYS_rt_sigprocmask,
                libc::SIG_SETMASK,
                &self.previous,
                std::ptr::null_mut::<u64>(),
                8,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mask() -> u64 {
        let mut mask = 0;
        assert_eq!(
            unsafe {
                libc::syscall(
                    libc::SYS_rt_sigprocmask,
                    libc::SIG_SETMASK,
                    std::ptr::null::<u64>(),
                    &mut mask,
                    8,
                )
            },
            0
        );
        mask
    }

    #[test]
    fn caller_mask_restores_on_success_and_early_refusal() {
        let original = mask();
        {
            let _blocked = Blocked::new().unwrap();
            let unmaskable = (1u64 << (libc::SIGKILL - 1)) | (1u64 << (libc::SIGSTOP - 1));
            assert_eq!(mask(), u64::MAX & !unmaskable);
        }
        assert_eq!(mask(), original);
        let refuse = || -> Result<()> {
            let _blocked = Blocked::new()?;
            anyhow::bail!("refused clone")
        };
        assert!(refuse().is_err());
        assert_eq!(mask(), original);
    }
}
