// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::key::exchange::handshake::fourway::{
    self, AuthenticatorKeyReplayCounter, Config, FourwayHandshakeFrame, SupplicantKeyReplayCounter,
};
use crate::key::exchange::{compute_mic_from_buf, Key};
use crate::key::gtk::Gtk;
use crate::key::igtk::Igtk;
use crate::key::ptk::Ptk;
use crate::key::Tk;
use crate::key_data::kde;
use crate::nonce::Nonce;
use crate::rsna::{
    derive_key_descriptor_version, Dot11VerifiedKeyFrame, NegotiatedProtection, SecAssocUpdate,
    UnverifiedKeyData, UpdateSink,
};
use crate::Error;
use anyhow::{ensure, format_err};
use log::{error, warn};
use std::fmt;
use zerocopy::SplitByteSlice;

#[derive(Debug, PartialEq)]
pub enum State {
    Idle {
        pmk: Vec<u8>,
        cfg: Config,
    },
    AwaitingMsg2 {
        pmk: Vec<u8>,
        cfg: Config,
        anonce: Nonce,
        key_replay_counter: AuthenticatorKeyReplayCounter,
    },
    AwaitingMsg4 {
        pmk: Vec<u8>,
        ptk: Ptk,
        gtk: Gtk,
        igtk: Option<Igtk>,
        cfg: Config,
        key_replay_counter: AuthenticatorKeyReplayCounter,
    },
    KeysInstalled {
        pmk: Vec<u8>,
        ptk: Ptk,
        gtk: Gtk,
        igtk: Option<Igtk>,
        cfg: Config,
    },
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            match self {
                State::Idle { .. } => "Idle",
                State::AwaitingMsg2 { .. } => "AwaitingMsg2",
                State::AwaitingMsg4 { .. } => "AwaitingMsg4",
                State::KeysInstalled { .. } => "KeysInstalled",
            }
        )
    }
}

pub fn new(cfg: Config, pmk: Vec<u8>) -> State {
    State::Idle { pmk, cfg }
}

impl State {
    #[allow(clippy::result_large_err, reason = "mass allow for https://fxbug.dev/381896734")]
    /// If [`self`] is in [`State::Idle`], then this function will push a
    /// [`SecAssocUpdate::TxEapolKeyFrame`] into [`update_sink`] and result [`self`]. Otherwise,
    /// [`self`] is dropped and an [`Error`] is returned.
    pub fn initiate(
        self,
        update_sink: &mut UpdateSink,
        s_key_replay_counter: SupplicantKeyReplayCounter,
    ) -> Result<Self, Error> {
        let key_replay_counter = AuthenticatorKeyReplayCounter::next_after(*s_key_replay_counter);
        match self {
            State::Idle { cfg, pmk } => {
                let anonce = cfg.nonce_rdr.next();
                match initiate_internal(update_sink, &cfg, key_replay_counter, &anonce[..]) {
                    Ok(()) => Ok(State::AwaitingMsg2 { anonce, cfg, pmk, key_replay_counter }),
                    Err(e) => Err(Error::GenericError(format!("{}", e))),
                }
            }
            other_state => Err(Error::GenericError(format!(
                "Failed to initiate Authenticator, currently in {} state",
                other_state
            ))),
        }
    }

    pub fn on_eapol_key_frame<B: SplitByteSlice>(
        self,
        update_sink: &mut UpdateSink,
        frame: FourwayHandshakeFrame<B>,
    ) -> Self {
        match self {
            State::Idle { cfg, pmk } => {
                error!("received EAPOL Key frame before initiate 4-Way Handshake");
                State::Idle { cfg, pmk }
            }
            State::AwaitingMsg2 { pmk, cfg, anonce, key_replay_counter } => {
                // Safe since the frame is only used for deriving the message number.
                match frame.message_number() {
                    fourway::MessageNumber::Message2 => {
                        match process_message_2(
                            update_sink,
                            &pmk[..],
                            &cfg,
                            &anonce[..],
                            key_replay_counter,
                            frame,
                        ) {
                            Ok((ptk, gtk, igtk, key_replay_counter)) => {
                                State::AwaitingMsg4 { pmk, ptk, gtk, igtk, cfg, key_replay_counter }
                            }
                            Err(e) => {
                                warn!("Unable to process second EAPOL handshake key frame from supplicant: {}", e);
                                State::AwaitingMsg2 { pmk, cfg, anonce, key_replay_counter }
                            }
                        }
                    }
                    unexpected_msg => {
                        error!(
                            "error: {:?}",
                            Error::UnexpectedHandshakeMessage(unexpected_msg.into())
                        );
                        State::AwaitingMsg2 { pmk, cfg, anonce, key_replay_counter }
                    }
                }
            }
            State::AwaitingMsg4 { pmk, ptk, gtk, igtk, cfg, key_replay_counter } => {
                match process_message_4(
                    update_sink,
                    &cfg,
                    &ptk,
                    &gtk,
                    &igtk,
                    key_replay_counter,
                    frame,
                ) {
                    Ok(()) => State::KeysInstalled { pmk, ptk, gtk, igtk, cfg },
                    Err(e) => {
                        error!("error: {}", e);
                        State::AwaitingMsg4 { pmk, ptk, gtk, igtk, cfg, key_replay_counter }
                    }
                }
            }
            other_state => other_state,
        }
    }

    #[allow(clippy::result_large_err, reason = "mass allow for https://fxbug.dev/381896734")]
    pub fn on_rsna_response_timeout(&self) -> Result<(), Error> {
        match self {
            State::AwaitingMsg2 { .. } => Err(Error::EapolHandshakeIncomplete(
                "Client never responded to EAPOL message 1".to_string(),
            )),
            State::AwaitingMsg4 { .. } => Err(Error::EapolHandshakeIncomplete(
                "Client never responded to EAPOL message 3".to_string(),
            )),
            _ => Ok(()),
        }
    }

    pub fn ptk(&self) -> Option<Ptk> {
        match self {
            State::AwaitingMsg4 { ptk, .. } | State::KeysInstalled { ptk, .. } => Some(ptk.clone()),
            _ => None,
        }
    }

    pub fn gtk(&self) -> Option<Gtk> {
        match self {
            State::AwaitingMsg4 { gtk, .. } | State::KeysInstalled { gtk, .. } => Some(gtk.clone()),
            _ => None,
        }
    }

    pub fn igtk(&self) -> Option<Igtk> {
        match self {
            State::AwaitingMsg4 { igtk, .. } | State::KeysInstalled { igtk, .. } => igtk.clone(),
            _ => None,
        }
    }

    pub fn destroy(self) -> Config {
        match self {
            State::Idle { cfg, .. } => cfg,
            State::AwaitingMsg2 { cfg, .. } => cfg,
            State::AwaitingMsg4 { cfg, .. } => cfg,
            State::KeysInstalled { cfg, .. } => cfg,
        }
    }
}

fn initiate_internal(
    update_sink: &mut UpdateSink,
    cfg: &Config,
    key_replay_counter: AuthenticatorKeyReplayCounter,
    anonce: &[u8],
) -> Result<(), anyhow::Error> {
    let protection = NegotiatedProtection::from_protection(&cfg.s_protection)?;
    let msg1 = create_message_1(anonce, &protection, key_replay_counter)?;
    update_sink.push(SecAssocUpdate::TxEapolKeyFrame { frame: msg1, expect_response: true });
    Ok(())
}

fn process_message_2<B: SplitByteSlice>(
    update_sink: &mut UpdateSink,
    pmk: &[u8],
    cfg: &Config,
    anonce: &[u8],
    key_replay_counter: AuthenticatorKeyReplayCounter,
    frame: FourwayHandshakeFrame<B>,
) -> Result<(Ptk, Gtk, Option<Igtk>, AuthenticatorKeyReplayCounter), anyhow::Error> {
    let ptk = handle_message_2(&pmk[..], &cfg, &anonce[..], key_replay_counter, frame)?;
    let gtk = cfg
        .gtk_provider
        .as_ref()
        // TODO(https://fxbug.dev/42104575): Replace with Error::MissingGtkProvider
        .ok_or_else(|| format_err!("GtkProvider is missing"))?
        .lock()
        .unwrap()
        .get_gtk()
        .clone();
    let igtk = match fourway::get_group_mgmt_cipher(&cfg.s_protection, &cfg.a_protection)
        .map_err(|e| format_err!("group mgmt cipher error: {:?}", e))?
    {
        Some(group_mgmt_cipher) => {
            let igtk_provider = cfg
                .igtk_provider
                .as_ref()
                // TODO(https://fxbug.dev/42104575): Replace with Error::MissingIgtkProvider
                .ok_or_else(|| format_err!("IgtkProvider is missing"))?
                .lock()
                .unwrap();
            let igtk_provider_cipher = igtk_provider.cipher();
            if group_mgmt_cipher != igtk_provider_cipher {
                // TODO(https://fxbug.dev/42104575): Replace with Error::WrongIgtkProviderCipher
                return Err(format_err!(
                    "wrong IgtkProvider cipher: {:?} != {:?}",
                    group_mgmt_cipher,
                    igtk_provider_cipher
                ));
            }
            Some(igtk_provider.get_igtk())
        }
        None => None,
    };
    let protection = NegotiatedProtection::from_protection(&cfg.s_protection)?;
    let key_replay_counter = AuthenticatorKeyReplayCounter::next_after(*key_replay_counter);
    let msg3 = create_message_3(
        &cfg,
        ptk.kck(),
        ptk.kek(),
        &gtk,
        &igtk,
        &anonce[..],
        &protection,
        key_replay_counter,
    )?;

    update_sink.push(SecAssocUpdate::TxEapolKeyFrame { frame: msg3, expect_response: true });
    Ok((ptk, gtk, igtk, key_replay_counter))
}

fn process_message_4<B: SplitByteSlice>(
    update_sink: &mut UpdateSink,
    cfg: &Config,
    ptk: &Ptk,
    gtk: &Gtk,
    igtk: &Option<Igtk>,
    key_replay_counter: AuthenticatorKeyReplayCounter,
    frame: FourwayHandshakeFrame<B>,
) -> Result<(), anyhow::Error> {
    handle_message_4(cfg, ptk.kck(), key_replay_counter, frame)?;
    update_sink.push(SecAssocUpdate::Key(Key::Ptk(ptk.clone())));
    update_sink.push(SecAssocUpdate::Key(Key::Gtk(gtk.clone())));
    if let Some(igtk) = igtk.as_ref() {
        update_sink.push(SecAssocUpdate::Key(Key::Igtk(igtk.clone())));
    }
    Ok(())
}

// IEEE Std 802.11-2016, 12.7.6.2
fn create_message_1<B: SplitByteSlice>(
    anonce: B,
    protection: &NegotiatedProtection,
    key_replay_counter: AuthenticatorKeyReplayCounter,
) -> Result<eapol::KeyFrameBuf, anyhow::Error> {
    let version = derive_key_descriptor_version(eapol::KeyDescriptor::IEEE802DOT11, protection);
    let key_info = eapol::KeyInformation(0)
        .with_key_descriptor_version(version)
        .with_key_type(eapol::KeyType::PAIRWISE)
        .with_key_ack(true);

    let key_len = match protection.pairwise.tk_bits() {
        None => {
            return Err(format_err!(
                "unknown cipher used for pairwise key: {:?}",
                protection.pairwise
            ))
        }
        Some(tk_bits) => tk_bits / 8,
    };
    eapol::KeyFrameTx::new(
        eapol::ProtocolVersion::IEEE802DOT1X2004,
        eapol::KeyFrameFields::new(
            eapol::KeyDescriptor::IEEE802DOT11,
            key_info,
            key_len,
            *key_replay_counter,
            eapol::to_array(&anonce),
            [0u8; 16], // iv
            0,         // rsc
        ),
        vec![],
        protection.mic_size as usize,
    )
    .serialize()
    .finalize_without_mic()
    .map_err(|e| e.into())
}

// IEEE Std 802.11-2016, 12.7.6.3
pub fn handle_message_2<B: SplitByteSlice>(
    pmk: &[u8],
    cfg: &Config,
    anonce: &[u8],
    key_replay_counter: AuthenticatorKeyReplayCounter,
    frame: FourwayHandshakeFrame<B>,
) -> Result<Ptk, anyhow::Error> {
    // Safe: The nonce must be accessed to compute the PTK. The frame will still be fully verified
    // before accessing any of its fields.
    let snonce = &frame.unsafe_get_raw().key_frame_fields.key_nonce[..];
    let protection = NegotiatedProtection::from_protection(&cfg.s_protection)?;

    let ptk = Ptk::new(
        pmk,
        &cfg.a_addr,
        &cfg.s_addr,
        anonce,
        snonce,
        &protection.akm,
        protection.pairwise.clone(),
    )?;

    // PTK was computed, verify the frame's MIC.
    let frame = match frame.get() {
        Dot11VerifiedKeyFrame::WithUnverifiedMic(unverified_mic) => {
            match unverified_mic.verify_mic(ptk.kck(), &protection)? {
                UnverifiedKeyData::Encrypted(_) => {
                    return Err(format_err!("msg2 of 4-Way Handshake must not be encrypted"))
                }
                UnverifiedKeyData::NotEncrypted(frame) => frame,
            }
        }
        Dot11VerifiedKeyFrame::WithoutMic(_) => {
            return Err(format_err!("msg2 of 4-Way Handshake must carry a MIC"))
        }
    };
    ensure!(
        frame.key_frame_fields.key_replay_counter.to_native() == *key_replay_counter,
        "error, expected Supplicant response to message {:?} but was {:?} in msg #2",
        *key_replay_counter,
        frame.key_frame_fields.key_replay_counter.to_native()
    );

    // TODO(hahnr): Key data must carry RSNE. Verify.

    Ok(ptk)
}

// IEEE Std 802.11-2016, 12.7.6.4
fn create_message_3(
    cfg: &Config,
    kck: &[u8],
    kek: &[u8],
    gtk: &Gtk,
    igtk: &Option<Igtk>,
    anonce: &[u8],
    protection: &NegotiatedProtection,
    key_replay_counter: AuthenticatorKeyReplayCounter,
) -> Result<eapol::KeyFrameBuf, anyhow::Error> {
    // Construct key data which contains the Beacon's RSNE and a GTK KDE.
    let mut w = kde::Writer::new();
    w.write_protection(&cfg.a_protection)?;
    w.write_gtk(&kde::Gtk::new(gtk.key_id(), kde::GtkInfoTx::BothRxTx, gtk.tk()))?;
    if let Some(igtk) = igtk.as_ref() {
        w.write_igtk(&kde::Igtk::new(igtk.key_id, &igtk.ipn, igtk.tk()))?;
    }
    let key_data = w.finalize_for_encryption()?;
    let key_iv = [0u8; 16];
    let encrypted_key_data =
        protection.keywrap_algorithm()?.wrap_key(kek, &key_iv, &key_data[..])?;

    // Construct message.
    let version = derive_key_descriptor_version(eapol::KeyDescriptor::IEEE802DOT11, protection);
    let key_info = eapol::KeyInformation(0)
        .with_key_descriptor_version(version)
        .with_key_type(eapol::KeyType::PAIRWISE)
        .with_key_ack(true)
        .with_key_mic(true)
        .with_install(true)
        .with_secure(true)
        .with_encrypted_key_data(true);

    let key_len = match protection.pairwise.tk_bits() {
        None => {
            return Err(format_err!(
                "unknown cipher used for pairwise key: {:?}",
                protection.pairwise
            ))
        }
        Some(tk_bits) => tk_bits / 8,
    };

    let msg3 = eapol::KeyFrameTx::new(
        eapol::ProtocolVersion::IEEE802DOT1X2004,
        eapol::KeyFrameFields::new(
            eapol::KeyDescriptor::IEEE802DOT11,
            key_info,
            key_len,
            *key_replay_counter,
            eapol::to_array(anonce),
            key_iv,
            gtk.key_rsc(),
        ),
        encrypted_key_data,
        protection.mic_size as usize,
    )
    .serialize();

    let mic = compute_mic_from_buf(kck, &protection, msg3.unfinalized_buf())
        .map_err(|e| anyhow::Error::from(e))?;
    msg3.finalize_with_mic(&mic[..]).map_err(|e| e.into())
}

// IEEE Std 802.11-2016, 12.7.6.5
pub fn handle_message_4<B: SplitByteSlice>(
    cfg: &Config,
    kck: &[u8],
    key_replay_counter: AuthenticatorKeyReplayCounter,
    frame: FourwayHandshakeFrame<B>,
) -> Result<(), anyhow::Error> {
    let protection = NegotiatedProtection::from_protection(&cfg.s_protection)?;
    let frame = match frame.get() {
        Dot11VerifiedKeyFrame::WithUnverifiedMic(unverified_mic) => {
            match unverified_mic.verify_mic(kck, &protection)? {
                UnverifiedKeyData::Encrypted(_) => {
                    return Err(format_err!("msg4 of 4-Way Handshake must not be encrypted"))
                }
                UnverifiedKeyData::NotEncrypted(frame) => frame,
            }
        }
        Dot11VerifiedKeyFrame::WithoutMic(_) => {
            return Err(format_err!("msg4 of 4-Way Handshake must carry a MIC"))
        }
    };
    ensure!(
        frame.key_frame_fields.key_replay_counter.to_native() == *key_replay_counter,
        "error, expected Supplicant response to message {:?} but was {:?} in msg #4",
        *key_replay_counter,
        frame.key_frame_fields.key_replay_counter.to_native()
    );

    // Note: The message's integrity was already verified by low layers.

    Ok(())
}
