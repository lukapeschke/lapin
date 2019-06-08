use log::trace;
use amq_protocol::frame::AMQPFrame;
use parking_lot::Mutex;

use std::{
  collections::{VecDeque, HashMap},
  sync::Arc,
};

use crate::{
  channel::Reply,
  id_sequence::IdSequence,
  wait::{Wait, WaitHandle},
};

pub type SendId = u64;

#[derive(Clone, Debug)]
pub enum Priority {
  LOW,
  NORMAL,
  HIGH,
  CRITICAL,
}

impl Default for Priority {
  fn default() -> Self {
    Priority::NORMAL
  }
}

#[derive(Clone, Debug, Default)]
pub struct Frames {
  inner: Arc<Mutex<Inner>>,
}

impl Frames {
  pub fn push(&self, channel_id: u16, priority: Priority, frame: AMQPFrame, expected_reply: Option<Reply>) -> Wait<()> {
    self.inner.lock().push(channel_id, priority, frame, expected_reply)
  }

  pub fn retry(&self, send_id: SendId, frame: AMQPFrame) {
    self.inner.lock().priority_frames.push_back((send_id, frame))
  }

  pub fn pop(&self) -> Option<(SendId, AMQPFrame)> {
    self.inner.lock().pop()
  }

  pub fn is_empty(&self) -> bool {
    self.inner.lock().is_empty()
  }

  pub fn next_expected_reply(&self, channel_id: u16) -> Option<Reply> {
    self.inner.lock().expected_replies.get_mut(&channel_id).and_then(|replies| replies.pop_front())
  }

  pub fn clear_expected_replies(&self, channel_id: u16) {
    self.inner.lock().expected_replies.remove(&channel_id);
  }

  pub fn mark_sent(&self, send_id: SendId) {
    if let Some(send) = self.inner.lock().outbox.remove(&send_id) {
      send.finish(());
    }
  }
}

#[derive(Debug)]
pub struct Inner {
  priority_frames:  VecDeque<(SendId, AMQPFrame)>,
  frames:           VecDeque<(SendId, AMQPFrame)>,
  low_prio_frames:  VecDeque<(SendId, AMQPFrame)>,
  expected_replies: HashMap<u16, VecDeque<Reply>>,
  outbox:           HashMap<SendId, WaitHandle<()>>,
  send_id:          IdSequence<SendId>,
}

impl Default for Inner {
  fn default() -> Self {
    Self {
      priority_frames:  VecDeque::default(),
      frames:           VecDeque::default(),
      low_prio_frames:  VecDeque::default(),
      expected_replies: HashMap::default(),
      outbox:           HashMap::default(),
      send_id:          IdSequence::new(false),
    }
  }
}

impl Inner {
  fn push(&mut self, channel_id: u16, priority: Priority, frame: AMQPFrame, expected_reply: Option<Reply>) -> Wait<()> {
    let send_id = if let Priority::CRITICAL = priority { 0 } else { self.send_id.next() };
    match priority {
      Priority::LOW      => self.low_prio_frames.push_back((send_id, frame)),
      Priority::NORMAL   => self.frames.push_back((send_id, frame)),
      Priority::HIGH     => self.priority_frames.push_back((send_id, frame)),
      Priority::CRITICAL => self.priority_frames.push_front((send_id, frame)),
    }
    let (wait, wait_handle) = Wait::new();
    self.outbox.insert(send_id, wait_handle);
    if let Some(reply) = expected_reply {
      trace!("channel {} state is now waiting for {:?}", channel_id, reply);
      self.expected_replies.entry(channel_id).or_default().push_back(reply);
    }
    wait
  }

  fn pop(&mut self) -> Option<(SendId, AMQPFrame)> {
    self.priority_frames.pop_front().or_else(|| self.frames.pop_front()).or_else(|| self.low_prio_frames.pop_front())
  }

  fn is_empty(&self) -> bool {
    self.priority_frames.is_empty() && self.frames.is_empty() && self.low_prio_frames.is_empty()
  }
}
