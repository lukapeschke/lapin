use crate::types::ShortString;
use log::trace;
use parking_lot::Mutex;
use std::{fmt, sync::Arc};

#[derive(Clone, Default)]
pub struct ChannelStatus {
    inner: Arc<Mutex<Inner>>,
}

impl ChannelStatus {
    pub fn is_initializing(&self) -> bool {
        self.inner.lock().state == ChannelState::Initial
    }

    pub fn is_closing(&self) -> bool {
        self.inner.lock().state == ChannelState::Closing
    }

    pub fn is_connected(&self) -> bool {
        !&[
            ChannelState::Initial,
            ChannelState::Closing,
            ChannelState::Closed,
            ChannelState::Error,
        ]
        .contains(&self.inner.lock().state)
    }

    pub fn confirm(&self) -> bool {
        self.inner.lock().confirm
    }

    pub(crate) fn set_confirm(&self) {
        self.inner.lock().confirm = true;
        trace!("Publisher confirms activated");
    }

    pub fn state(&self) -> ChannelState {
        self.inner.lock().state.clone()
    }

    pub(crate) fn set_state(&self, state: ChannelState) {
        self.inner.lock().state = state
    }

    pub(crate) fn set_send_flow(&self, flow: bool) {
        self.inner.lock().send_flow = flow;
    }

    pub(crate) fn flow(&self) -> bool {
        self.inner.lock().send_flow
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChannelState {
    Initial,
    Connected,
    Closing,
    Closed,
    Error,
    SendingContent(usize),
    WillReceiveContent(Option<ShortString>, Option<ShortString>),
    ReceivingContent(Option<ShortString>, Option<ShortString>, usize),
}

impl Default for ChannelState {
    fn default() -> Self {
        ChannelState::Initial
    }
}

impl fmt::Debug for ChannelStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = f.debug_struct("ChannelStatus");
        if let Some(inner) = self.inner.try_lock() {
            debug
                .field("state", &inner.state)
                .field("confirm", &inner.confirm)
                .field("send_flow", &inner.send_flow);
        }
        debug.finish()
    }
}

struct Inner {
    confirm: bool,
    send_flow: bool,
    state: ChannelState,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            confirm: false,
            send_flow: true,
            state: ChannelState::default(),
        }
    }
}
