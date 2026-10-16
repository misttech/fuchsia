// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::collections::VecDeque;

use super::message_types::{AncillaryData, Message, MessageData, UnixControlData};
use crate::vfs::buffers::{InputBuffer, OutputBuffer};
use crate::vfs::socket::SocketAddress;
use starnix_uapi::errors::Errno;
use starnix_uapi::vfs::FdEvents;
use starnix_uapi::{error, uapi};

/// Controls whether `SO_PASSCRED` is enabled, which causes stream reads and peeks to break before
/// messages with differing credentials.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum PasscredMode {
    #[default]
    Disabled,
    Enabled,
}

impl PasscredMode {
    pub fn is_enabled(self) -> bool {
        matches!(self, Self::Enabled)
    }
}

impl From<bool> for PasscredMode {
    fn from(enabled: bool) -> Self {
        if enabled { Self::Enabled } else { Self::Disabled }
    }
}

#[derive(Debug, Default, Clone)]
pub struct MessageReadInfo {
    pub bytes_read: usize,
    pub message_length: usize,
    pub address: Option<SocketAddress>,
    pub credentials: Option<uapi::ucred>,
    pub ancillary_data: Vec<AncillaryData>,
}

impl MessageReadInfo {
    /// Appends `info` to self.
    pub fn append(&mut self, info: &mut MessageReadInfo) {
        self.bytes_read += info.bytes_read;
        self.message_length += info.message_length;
        if self.address.is_none() {
            self.address = info.address.take();
        }
        if self.credentials.is_none() {
            self.credentials = info.credentials.take();
        }
        if self.ancillary_data.iter().any(AncillaryData::is_credentials) {
            info.ancillary_data.retain(|d| !d.is_credentials());
        }
        self.ancillary_data.append(&mut info.ancillary_data);
    }
}

/// A `MessageQueue` stores a FIFO sequence of messages.
#[derive(Debug)]
pub struct MessageQueue<D: MessageData = Vec<u8>> {
    /// The messages stored in the message queue.
    ///
    /// Writes are added at the end of the queue. Reads consume from the front of the queue.
    messages: VecDeque<Message<D>>,

    /// The total number of bytes currently in the message queue.
    length: usize,

    /// The maximum number of bytes that can be stored inside this pipe.
    capacity: usize,
}

impl<D: MessageData> MessageQueue<D> {
    pub fn new(capacity: usize) -> Self {
        MessageQueue { messages: VecDeque::default(), length: 0, capacity }
    }

    /// Returns the number of bytes that can be written to the message queue before the buffer is
    /// full.
    pub fn available_capacity(&self) -> usize {
        self.capacity - self.length
    }

    /// Returns the total number of bytes this message queue can store, regardless of the current
    /// amount of data in the buffer.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn messages(&self) -> impl Iterator<Item = &Message<D>> {
        self.messages.iter()
    }

    /// Sets the capacity of the message queue to the provided number of bytes.
    ///
    /// Reurns an error if the requested capacity could not be set (e.g., if the requested capacity
    /// was less than the current number of bytes stored).
    pub fn set_capacity(&mut self, requested_capacity: usize) -> Result<(), Errno> {
        if requested_capacity < self.length {
            return error!(EBUSY);
        }
        self.capacity = requested_capacity;
        Ok(())
    }

    /// Returns true if the message queue is empty, or it only contains empty messages.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the total length of all the messages in the message queue.
    pub fn len(&self) -> usize {
        self.length
    }

    fn update_address(message: &Message<D>, address: &mut Option<SocketAddress>) -> bool {
        if message.address.is_some() && *address != message.address {
            if address.is_some() {
                return false;
            }
            *address = message.address.clone();
        }
        true
    }

    fn update_creds(
        passcred: PasscredMode,
        message: &Message<D>,
        creds: &mut Option<uapi::ucred>,
    ) -> bool {
        if passcred.is_enabled() {
            let message_creds = message.credentials.unwrap_or_else(UnixControlData::unknown_ucred);
            if let Some(existing_creds) = *creds {
                if existing_creds != message_creds {
                    return false;
                }
            } else {
                *creds = Some(message_creds);
            }
        }
        true
    }

    /// Returns whether the next message in the queue (if any) can be coalesced into `info`.
    pub fn can_coalesce_with(&self, info: &MessageReadInfo, passcred: PasscredMode) -> bool {
        if info.bytes_read == 0 {
            return true;
        }
        if info.ancillary_data.iter().any(|d| !d.is_credentials()) {
            return false;
        }
        if passcred.is_enabled() != info.credentials.is_some() {
            return false;
        }
        let Some(message) = self.peek_message() else {
            return true;
        };
        let mut address = info.address.clone();
        let mut creds = info.credentials;
        Self::update_address(message, &mut address)
            && Self::update_creds(passcred, message, &mut creds)
    }

    /// Reads messages until there are no more messages, a message with ancillary data is
    /// encountered, or `data` are full.
    ///
    /// To read data from the queue without consuming the messages, see `peek_stream`.
    ///
    /// # Parameters
    /// - `data`: The `OutputBuffer` to write the data to.
    ///
    /// Returns the message information containing the number of bytes read, the address, and any
    /// ancillary data. Also returns a boolean indicating if any messages were read.
    pub fn read_stream(
        &mut self,
        data: &mut dyn OutputBuffer,
    ) -> Result<(MessageReadInfo, bool), Errno> {
        self.read_stream_with_passcred(data, PasscredMode::Disabled)
    }

    /// Reads messages from a stream until there are no more messages, a message with ancillary
    /// data is encountered, a message with differing credentials is encountered (when `passcred`
    /// is `PasscredMode::Enabled`), or `data` are full.
    ///
    /// # Parameters
    /// - `data`: The `OutputBuffer` to write the data to.
    /// - `passcred`: Whether `SO_PASSCRED` is enabled on the receiving socket.
    pub fn read_stream_with_passcred(
        &mut self,
        data: &mut dyn OutputBuffer,
        passcred: PasscredMode,
    ) -> Result<(MessageReadInfo, bool), Errno> {
        let mut total_bytes_read = 0;
        let mut address = None;
        let mut ancillary_data = vec![];
        let mut creds = None;

        let mut messages_read = 0;
        loop {
            let mut message = match self.read_message() {
                Some(m) => m,
                None => break,
            };
            if !Self::update_address(&message, &mut address)
                || !Self::update_creds(passcred, &message, &mut creds)
            {
                // We've already locked onto an address or credentials for this batch of messages,
                // but we have found a message that doesn't match. We put it back for now and
                // return the messages we have so far.
                self.write_front(message);
                break;
            }
            messages_read += 1;

            let bytes_read = message.data.copy_to_user(data)?;
            total_bytes_read += bytes_read;

            if let Some(remaining_message) = message.split_off(bytes_read) {
                // If not all the message data could fit, return the ancillary data now,
                // and put the remaining data back with its address and credentials preserved.
                ancillary_data.append(&mut message.ancillary_data);
                self.write_front(remaining_message);
                break;
            }

            if !message.ancillary_data.is_empty() {
                ancillary_data.append(&mut message.ancillary_data);
                break;
            }

            if data.available() == 0 {
                break;
            }
        }

        Ok((
            MessageReadInfo {
                bytes_read: total_bytes_read,
                message_length: total_bytes_read,
                address,
                credentials: creds,
                ancillary_data,
            },
            messages_read > 0,
        ))
    }

    /// Peeks messages until there are no more messages, a message with ancillary data is
    /// encountered, a message with differing credentials is encountered (when `passcred` is
    /// `PasscredMode::Enabled`), or `data` are full.
    ///
    /// Unlike `read_stream`, this function does not remove the messages from the queue.
    ///
    /// Used to implement MSG_PEEK.
    ///
    /// # Parameters
    /// - `data`: The `OutputBuffer` to write the data to.
    /// - `passcred`: Whether `SO_PASSCRED` is enabled on the receiving socket.
    ///
    /// Returns the message information containing the number of bytes read, the address, and any
    /// ancillary data. Also returns a boolean indicating if any messages were read.
    pub fn peek_stream(
        &self,
        data: &mut dyn OutputBuffer,
        passcred: PasscredMode,
    ) -> Result<(MessageReadInfo, bool), Errno> {
        let mut total_bytes_read = 0;
        let mut address = None;
        let mut ancillary_data = vec![];
        let mut creds = None;

        let mut messages_peeked = 0;
        for (index, message) in self.messages.iter().enumerate() {
            if index > 0 && data.available() == 0 {
                break;
            }

            if !Self::update_address(message, &mut address)
                || !Self::update_creds(passcred, message, &mut creds)
            {
                break;
            }
            messages_peeked += 1;

            ancillary_data.extend(message.ancillary_data.iter().cloned());

            let bytes_read = message.data.copy_to_user(data)?;
            total_bytes_read += bytes_read;

            if bytes_read < message.len() || !message.ancillary_data.is_empty() {
                break;
            }
        }

        Ok((
            MessageReadInfo {
                bytes_read: total_bytes_read,
                message_length: total_bytes_read,
                address,
                credentials: creds,
                ancillary_data,
            },
            messages_peeked > 0,
        ))
    }

    pub fn read_datagram(
        &mut self,
        data: &mut dyn OutputBuffer,
    ) -> Result<(MessageReadInfo, bool), Errno> {
        if let Some(message) = self.read_message() {
            Ok((
                MessageReadInfo {
                    bytes_read: message.data.copy_to_user(data)?,
                    message_length: message.len(),
                    address: message.address,
                    credentials: message.credentials,
                    ancillary_data: message.ancillary_data,
                },
                true,
            ))
        } else {
            Ok((MessageReadInfo::default(), false))
        }
    }

    pub fn peek_datagram(
        &mut self,
        data: &mut dyn OutputBuffer,
    ) -> Result<(MessageReadInfo, bool), Errno> {
        if let Some(message) = self.peek_message() {
            Ok((
                MessageReadInfo {
                    bytes_read: message.data.copy_to_user(data)?,
                    message_length: message.len(),
                    address: message.address.clone(),
                    credentials: message.credentials,
                    ancillary_data: message.ancillary_data.clone(),
                },
                true,
            ))
        } else {
            Ok((MessageReadInfo::default(), false))
        }
    }

    /// Reads the next message in the buffer, if such a message exists.
    pub fn read_message(&mut self) -> Option<Message<D>> {
        self.messages.pop_front().map(|message| {
            self.length -= message.len();
            message
        })
    }

    pub fn peek_queue(&self) -> &VecDeque<Message<D>> {
        &self.messages
    }

    /// Peeks the next message in the buffer, if such a message exists.
    fn peek_message(&self) -> Option<&Message<D>> {
        self.messages.front()
    }

    /// Writes the the contents of `InputBuffer` into this socket.
    /// Will return EAGAIN if not enough capacity is available.
    ///
    /// # Parameters
    /// - `task`: The task to read memory from.
    /// - `data`: The `InputBuffer` to read the data from.
    ///
    /// Returns the number of bytes that were written to the socket.
    pub fn write_stream(
        &mut self,
        data: &mut dyn InputBuffer,
        address: Option<SocketAddress>,
        ancillary_data: &mut Vec<AncillaryData>,
    ) -> Result<usize, Errno> {
        self.write_stream_with_filter(data, address, None, ancillary_data, Some)
    }

    /// Writes the the contents of `InputBuffer` into this socket.
    /// Will return EAGAIN if not enough capacity is available.
    ///
    /// # Parameters
    /// - `task`: The task to read memory from.
    /// - `data`: The `InputBuffer` to read the data from.
    /// - `filter`: A filter to run on the message before inserting it into the queue. If it
    ///             returns None, no message is enqueued.
    ///
    /// Returns the number of bytes that were written to the socket.
    pub fn write_stream_with_filter(
        &mut self,
        data: &mut dyn InputBuffer,
        address: Option<SocketAddress>,
        default_credentials: Option<uapi::ucred>,
        ancillary_data: &mut Vec<AncillaryData>,
        filter: impl FnOnce(Message<D>) -> Option<Message<D>>,
    ) -> Result<usize, Errno> {
        let actual = std::cmp::min(self.available_capacity(), data.available());
        if actual == 0 && data.available() > 0 {
            return error!(EAGAIN);
        }
        let message_data = MessageData::copy_from_user(data, actual)?;
        let explicit_creds = ancillary_data.iter().rev().find_map(AncillaryData::credentials);
        let mut message_ancillary_data = std::mem::take(ancillary_data);
        message_ancillary_data.retain(|d| !d.is_credentials());
        if data.available() > 0 {
            if let Some(creds) = explicit_creds {
                ancillary_data.push(AncillaryData::Unix(UnixControlData::Credentials(creds)));
            }
        }
        let message = Message::new(
            message_data,
            address,
            explicit_creds.or(default_credentials),
            message_ancillary_data,
        );
        if let Some(message) = filter(message) {
            if message.len() > 0
                || !message.ancillary_data.is_empty()
                || message.credentials.is_some()
                || message.address.is_some()
            {
                self.write_message(message);
            }
        }
        Ok(actual)
    }

    /// Writes the the contents of `InputBuffer` into this socket as
    /// single message. Will return EAGAIN if not enough capacity is available.
    ///
    /// # Parameters
    /// - `task`: The task to read memory from.
    /// - `data`: The `InputBuffer` to read the data from.
    ///
    /// Returns the number of bytes that were written to the socket.
    pub fn write_datagram(
        &mut self,
        data: &mut dyn InputBuffer,
        address: Option<SocketAddress>,
        default_credentials: Option<uapi::ucred>,
        ancillary_data: &mut Vec<AncillaryData>,
    ) -> Result<usize, Errno> {
        self.write_datagram_with_filter(data, address, default_credentials, ancillary_data, Some)
    }

    /// Writes the the contents of `InputBuffer` into this socket as
    /// single message. Will return EAGAIN if not enough capacity is available.
    ///
    /// # Parameters
    /// - `task`: The task to read memory from.
    /// - `data`: The `InputBuffer` to read the data from.
    /// - `filter`: A filter to run on the message before inserting it into the queue. If it
    ///             returns None, no message is enqueued.
    ///
    /// Returns the number of bytes that were written to the socket.
    pub fn write_datagram_with_filter(
        &mut self,
        data: &mut dyn InputBuffer,
        address: Option<SocketAddress>,
        default_credentials: Option<uapi::ucred>,
        ancillary_data: &mut Vec<AncillaryData>,
        filter: impl FnOnce(Message<D>) -> Option<Message<D>>,
    ) -> Result<usize, Errno> {
        let actual = data.available();
        if actual > self.capacity() {
            return error!(EMSGSIZE);
        }
        if actual > self.available_capacity() {
            return error!(EAGAIN);
        }
        let message_data = MessageData::copy_from_user(data, actual)?;
        let explicit_creds = ancillary_data.iter().rev().find_map(AncillaryData::credentials);
        let mut message_ancillary_data = std::mem::take(ancillary_data);
        message_ancillary_data.retain(|d| !d.is_credentials());
        let message = Message::new(
            message_data,
            address,
            explicit_creds.or(default_credentials),
            message_ancillary_data,
        );
        if let Some(message) = filter(message) {
            self.write_message(message);
        }
        Ok(actual)
    }

    /// Writes a message to the front of the message queue.
    pub fn write_front(&mut self, message: Message<D>) {
        self.length += message.len();
        debug_assert!(self.length <= self.capacity);
        self.messages.push_front(message);
    }

    /// Writes a message to the back of the message queue.
    pub fn write_message(&mut self, message: Message<D>) {
        self.length += message.len();
        debug_assert!(self.length <= self.capacity);
        self.messages.push_back(message);
    }

    pub fn query_events(&self) -> FdEvents {
        let mut events = FdEvents::empty();
        if self.available_capacity() > 0 {
            events |= FdEvents::POLLOUT;
        }
        if !self.is_empty() {
            events |= FdEvents::POLLIN;
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests that a write followed by a read returns the written message.
    #[::fuchsia::test]
    fn test_read_write() {
        let mut message_queue = MessageQueue::new(usize::MAX);
        let bytes: Vec<u8> = vec![1, 2, 3];
        let message: Message = bytes.into();
        message_queue.write_message(message.clone());
        assert_eq!(message_queue.len(), 3);
        assert_eq!(message_queue.read_message(), Some(message));
        assert!(message_queue.is_empty());
    }

    /// Tests that ancillary data does not contribute to the message queue length.
    #[::fuchsia::test]
    fn test_control_len() {
        let mut message_queue = MessageQueue::new(usize::MAX);
        let bytes: Vec<u8> = vec![1, 2, 3];
        let ancillary_data =
            vec![AncillaryData::Unix(UnixControlData::Security(bytes.clone().into()))];
        let message = Message::new(vec![].into(), None, None, ancillary_data);
        message_queue.write_message(message);
        assert_eq!(message_queue.len(), 0);
        message_queue.write_message(bytes.clone().into());
        assert_eq!(message_queue.len(), bytes.len());
    }

    /// Tests that multiple writes followed by multiple reads return the data in the correct order.
    #[::fuchsia::test]
    fn test_read_write_multiple() {
        let mut message_queue = MessageQueue::new(usize::MAX);
        let first_bytes: Vec<u8> = vec![1, 2, 3];
        let second_bytes: Vec<u8> = vec![3, 4, 5];

        for message in [first_bytes.clone().into(), second_bytes.clone().into()] {
            message_queue.write_message(message);
        }

        assert_eq!(message_queue.len(), first_bytes.len() + second_bytes.len());
        assert_eq!(message_queue.read_message(), Some(first_bytes.into()));
        assert_eq!(message_queue.len(), second_bytes.len());
        assert_eq!(message_queue.read_message(), Some(second_bytes.into()));
        assert_eq!(message_queue.read_message(), None);
    }

    #[::fuchsia::test]
    fn test_can_coalesce_with_passcred_toggled() {
        let mut message_queue: MessageQueue = MessageQueue::new(usize::MAX);
        let creds = uapi::ucred { pid: 1, uid: 2, gid: 3 };
        message_queue.write_message(Message::new(vec![1, 2].into(), None, Some(creds), Vec::new()));

        let info_with_creds = MessageReadInfo {
            bytes_read: 2,
            message_length: 2,
            address: None,
            credentials: Some(creds),
            ancillary_data: Vec::new(),
        };
        assert!(message_queue.can_coalesce_with(&info_with_creds, PasscredMode::Enabled));
        assert!(!message_queue.can_coalesce_with(&info_with_creds, PasscredMode::Disabled));

        let info_without_creds = MessageReadInfo {
            bytes_read: 2,
            message_length: 2,
            address: None,
            credentials: None,
            ancillary_data: Vec::new(),
        };
        assert!(message_queue.can_coalesce_with(&info_without_creds, PasscredMode::Disabled));
        assert!(!message_queue.can_coalesce_with(&info_without_creds, PasscredMode::Enabled));
    }
}
