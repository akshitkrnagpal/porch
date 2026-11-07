//! Password-authenticated key exchange for code pairing (SPAKE2 over Ed25519).
//!
//! The 6-digit code never crosses the network. Both sides derive a shared key
//! from it; an attacker who does not know the code gets one guess per exchange
//! and learns nothing that allows testing other codes offline. Each side proves
//! the key (and the host certificate the joiner pinned) before the other side
//! releases anything: the joiner proves first and the host counts failures, then
//! the host proves before the joiner turns on sharing or mints a credential.
use super::types::Machine;
use anyhow::{bail, Result};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use spake2::{Ed25519Group, Identity, Password, Spake2};

type HmacSha256 = Hmac<Sha256>;
const JOINER: &[u8] = b"porch-pair-v2-joiner";
const HOST: &[u8] = b"porch-pair-v2-host";

/// The SPAKE2 password: the code salted with the window's public salt.
pub(super) fn password(salt: &str, code: &str) -> String {
    super::server::hex(&Sha256::digest(format!("{salt}\0{code}").as_bytes()))
}

pub(super) struct Key([u8; 32]);

pub(super) struct JoinerState(Spake2<Ed25519Group>);
pub(super) fn joiner_start(password: &str) -> (JoinerState, String) {
    let (state, msg) = Spake2::<Ed25519Group>::start_a(
        &Password::new(password.as_bytes()),
        &Identity::new(JOINER),
        &Identity::new(HOST),
    );
    (JoinerState(state), super::server::hex(&msg))
}
pub(super) fn joiner_finish(state: JoinerState, host_msg: &str) -> Result<Key> {
    key(state.0, host_msg)
}
/// Host side: answer the joiner's message; returns the host's message and the key.
pub(super) fn host_respond(password: &str, joiner_msg: &str) -> Result<(String, Key)> {
    let (state, msg) = Spake2::<Ed25519Group>::start_b(
        &Password::new(password.as_bytes()),
        &Identity::new(JOINER),
        &Identity::new(HOST),
    );
    Ok((super::server::hex(&msg), key(state, joiner_msg)?))
}
fn key(state: Spake2<Ed25519Group>, msg: &str) -> Result<Key> {
    let msg = unhex(msg)?;
    if msg.len() != 33 {
        bail!("invalid pairing message")
    }
    let shared = state
        .finish(&msg)
        .map_err(|_| anyhow::anyhow!("invalid pairing message"))?;
    let mut out = [0u8; 32];
    out.copy_from_slice(&Sha256::digest(&shared));
    Ok(Key(out))
}

pub(super) fn unhex(value: &str) -> Result<Vec<u8>> {
    if !value.len().is_multiple_of(2)
        || value.len() > 256
        || !value.bytes().all(|b| b.is_ascii_hexdigit())
    {
        bail!("invalid hex value")
    }
    (0..value.len())
        .step_by(2)
        .map(|i| Ok(u8::from_str_radix(&value[i..i + 2], 16)?))
        .collect()
}

impl Key {
    fn mac(&self, label: &str, parts: &[&[u8]]) -> HmacSha256 {
        let mut mac = HmacSha256::new_from_slice(&self.0).expect("HMAC accepts any key length");
        for part in std::iter::once(label.as_bytes()).chain(parts.iter().copied()) {
            mac.update(&(part.len() as u64).to_be_bytes());
            mac.update(part);
        }
        mac
    }
    fn tag(&self, label: &str, parts: &[&[u8]]) -> String {
        super::server::hex(&self.mac(label, parts).finalize().into_bytes())
    }
    fn check(&self, label: &str, parts: &[&[u8]], tag: &str) -> bool {
        unhex(tag).is_ok_and(|t| self.mac(label, parts).verify_slice(&t).is_ok())
    }
    /// Joiner's key confirmation, bound to the host certificate it pinned.
    pub(super) fn joiner_confirm(&self, host_pin: &[u8; 32]) -> String {
        self.tag("joiner-confirm", &[host_pin])
    }
    pub(super) fn check_joiner_confirm(&self, host_pin: &[u8; 32], tag: &str) -> bool {
        self.check("joiner-confirm", &[host_pin], tag)
    }
    pub(super) fn host_confirm(&self, host_pin: &[u8; 32]) -> String {
        self.tag("host-confirm", &[host_pin])
    }
    pub(super) fn check_host_confirm(&self, host_pin: &[u8; 32], tag: &str) -> bool {
        self.check("host-confirm", &[host_pin], tag)
    }
    /// Authenticates the joiner's credentials independently of the TLS channel.
    pub(super) fn joiner_payload(
        &self,
        machine: &Machine,
        endpoint: &str,
        fingerprint: &str,
        token: &str,
    ) -> String {
        self.tag(
            "joiner-payload",
            &payload(machine, endpoint, fingerprint, token),
        )
    }
    pub(super) fn check_joiner_payload(
        &self,
        machine: &Machine,
        endpoint: &str,
        fingerprint: &str,
        token: &str,
        tag: &str,
    ) -> bool {
        self.check(
            "joiner-payload",
            &payload(machine, endpoint, fingerprint, token),
            tag,
        )
    }
    pub(super) fn host_payload(&self, machine: &Machine, token: &str) -> String {
        self.tag(
            "host-payload",
            &[
                machine.id.as_bytes(),
                machine.name.as_bytes(),
                token.as_bytes(),
            ],
        )
    }
    pub(super) fn check_host_payload(&self, machine: &Machine, token: &str, tag: &str) -> bool {
        self.check(
            "host-payload",
            &[
                machine.id.as_bytes(),
                machine.name.as_bytes(),
                token.as_bytes(),
            ],
            tag,
        )
    }
}
fn payload<'a>(
    machine: &'a Machine,
    endpoint: &'a str,
    fingerprint: &'a str,
    token: &'a str,
) -> [&'a [u8]; 5] {
    [
        machine.id.as_bytes(),
        machine.name.as_bytes(),
        endpoint.as_bytes(),
        fingerprint.as_bytes(),
        token.as_bytes(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn same_code_agrees_and_a_wrong_code_or_pin_does_not() {
        let pin = [7u8; 32];
        let pw = password("salt", "123456");
        let (joiner, msg_a) = joiner_start(&pw);
        let (msg_b, host_key) = host_respond(&pw, &msg_a).unwrap();
        let joiner_key = joiner_finish(joiner, &msg_b).unwrap();
        assert!(host_key.check_joiner_confirm(&pin, &joiner_key.joiner_confirm(&pin)));
        assert!(joiner_key.check_host_confirm(&pin, &host_key.host_confirm(&pin)));
        // Roles are not interchangeable: a reflected confirmation fails.
        assert!(!joiner_key.check_host_confirm(&pin, &joiner_key.joiner_confirm(&pin)));
        // A relaying interceptor presents another certificate.
        assert!(!host_key.check_joiner_confirm(&[8u8; 32], &joiner_key.joiner_confirm(&pin)));

        let (joiner, msg_a) = joiner_start(&password("salt", "654321"));
        let (msg_b, host_key) = host_respond(&pw, &msg_a).unwrap();
        let joiner_key = joiner_finish(joiner, &msg_b).unwrap();
        assert!(!host_key.check_joiner_confirm(&pin, &joiner_key.joiner_confirm(&pin)));
        assert!(!joiner_key.check_host_confirm(&pin, &host_key.host_confirm(&pin)));
        // The code itself is not derivable from the exchanged messages.
        assert!(!msg_a.contains("123456") && !msg_b.contains("123456"));
    }
    #[test]
    fn malformed_messages_are_rejected() {
        let pw = password("salt", "123456");
        for bad in ["", "zz", &"00".repeat(33), &"ab".repeat(200)] {
            assert!(host_respond(&pw, bad).is_err(), "{bad}");
        }
    }
}
