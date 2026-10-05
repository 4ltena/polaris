//! Account commands never put OAuth codes or token values into TUI history.
use crate::{approver::KeyReader, render};
use polaris_core::{
    management_effect::{
        ManagementEffectStoreV1,
        account_change::{AccountChangeOwnerV1, AccountChangeV1},
    },
    rewind_workspace::WorkspaceOwner,
};
use ratatui::{Terminal, backend::Backend, text::Line};
use std::{cell::RefCell, sync::Arc, time::Duration};

/// Supplied by the CLI's existing token owner; every hosted provider shares it.
pub trait BindingControl: Send + Sync {
    fn current(&self) -> Option<(String, u64)>;
    fn bind(&self, registration: &str) -> Result<String, String>;
}

pub(crate) fn confirm<B: Backend, R: KeyReader>(
    terminal: &RefCell<Terminal<B>>,
    reader: &mut R,
    operation: &str,
    target: &str,
    detail: &str,
) -> bool {
    loop {
        let mut ready = false;
        if terminal
            .borrow_mut()
            .draw(|frame| {
                ready = render::render_management_confirmation(frame, operation, target, detail);
            })
            .is_err()
        {
            return false;
        }
        match reader.read_key() {
            Ok(ratatui::crossterm::event::KeyCode::Esc) | Err(_) => return false,
            Ok(ratatui::crossterm::event::KeyCode::Char('y')) => {
                return ready;
            }
            _ => {}
        }
    }
}
