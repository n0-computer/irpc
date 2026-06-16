//! No channels, used when no communication is needed

use crate::sealed::Sealed;

/// A sender that does nothing. This is used when no communication is needed.
#[derive(Debug)]
pub struct NoSender;
impl Sealed for NoSender {}
impl crate::Sender for NoSender {}

/// A receiver that does nothing. This is used when no communication is needed.
#[derive(Debug)]
pub struct NoReceiver;

impl Sealed for NoReceiver {}
impl crate::Receiver for NoReceiver {}
