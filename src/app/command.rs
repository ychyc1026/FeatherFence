use std::cell::Cell;
use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

pub(crate) const WM_APP_DISPATCH: u32 = WM_APP + 30;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AppCommand {
    SweepDesktop,
    RefreshFence { id: u32 },
}

#[derive(Default)]
struct CommandQueue {
    pending: VecDeque<AppCommand>,
}

impl CommandQueue {
    fn push(&mut self, command: AppCommand) {
        self.pending.push_back(command);
    }

    fn pop(&mut self) -> Option<AppCommand> {
        self.pending.pop_front()
    }
}

static COMMANDS: OnceLock<Mutex<CommandQueue>> = OnceLock::new();

thread_local! {
    static DISPATCHING: Cell<bool> = const { Cell::new(false) };
}

struct DispatchGuard;

impl DispatchGuard {
    fn enter() -> Option<Self> {
        DISPATCHING.with(|dispatching| {
            if dispatching.replace(true) {
                None
            } else {
                Some(Self)
            }
        })
    }
}

impl Drop for DispatchGuard {
    fn drop(&mut self) {
        DISPATCHING.with(|dispatching| dispatching.set(false));
    }
}

fn queue() -> &'static Mutex<CommandQueue> {
    COMMANDS.get_or_init(|| Mutex::new(CommandQueue::default()))
}

pub(crate) fn post(hwnd: HWND, command: AppCommand) {
    queue()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(command);
    if unsafe { PostMessageW(Some(hwnd), WM_APP_DISPATCH, WPARAM(0), LPARAM(0)) }.is_err() {
        crate::dlog("[command] failed to post WM_APP_DISPATCH");
    }
}

pub(crate) fn drain(mut apply: impl FnMut(AppCommand)) {
    let Some(_guard) = DispatchGuard::enter() else {
        return;
    };
    loop {
        let command = queue()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pop();
        let Some(command) = command else {
            break;
        };
        apply(command);
    }
}

#[cfg(test)]
mod tests {
    use super::{AppCommand, CommandQueue};

    #[test]
    fn commands_are_drained_in_post_order() {
        let mut queue = CommandQueue::default();
        queue.push(AppCommand::RefreshFence { id: 7 });
        queue.push(AppCommand::SweepDesktop);

        assert_eq!(queue.pop(), Some(AppCommand::RefreshFence { id: 7 }));
        assert_eq!(queue.pop(), Some(AppCommand::SweepDesktop));
        assert_eq!(queue.pop(), None);
    }
}
