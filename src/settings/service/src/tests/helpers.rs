// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::base::SettingType;
use crate::event::Publisher;
use crate::message::base::MessengerType;
use crate::message::delegate::Delegate;
use crate::message::messenger::MessengerClient;
use crate::message::receptor::Receptor;
use crate::service::{Address as ServiceAddress, MessageHub};

// Create a messenger hub, returning an unbound messenger and publisher.
pub async fn create_messenger_and_publisher() -> (MessengerClient, Publisher) {
    let message_hub = MessageHub::create_hub();
    let publisher = Publisher::create(&message_hub, MessengerType::Unbound).await;

    let messenger =
        message_hub.create(MessengerType::Unbound).await.expect("Unable to create messenger").0;

    (messenger, publisher)
}

// Create and return an unbound messenger and publisher from a given `message_hub`.
pub async fn create_messenger_and_publisher_from_hub(
    message_hub: &Delegate,
) -> (MessengerClient, Publisher) {
    let publisher = Publisher::create(message_hub, MessengerType::Unbound).await;
    let messenger =
        message_hub.create(MessengerType::Unbound).await.expect("Unable to create messenger").0;

    (messenger, publisher)
}

// Given a `setting_type` and `message_hub`, creates a receptor from the message hub with the address
// of the setting type.
pub async fn create_receptor_for_setting_type(
    message_hub: &Delegate,
    setting_type: SettingType,
) -> Receptor {
    message_hub
        .create(MessengerType::Addressable(ServiceAddress::Handler(setting_type)))
        .await
        .expect("Unable to create receptor")
        .1
}
